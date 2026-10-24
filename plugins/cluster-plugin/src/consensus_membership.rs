//! Production voter-set reconciliation for membership workflows.

use crate::consensus_network::ConsensusNodeRegistry;
use crate::consensus_runtime::{ConsensusNode, ConsensusWriteError};
use crate::membership::NodeId;
use bmux_cluster_plugin_api::cluster_types::{
    ClusterConsensusRole, ClusterMember, ClusterMemberState, ConsensusVoterChangeAction,
    ConsensusVoterChangeAuthorization,
};
use openraft::BasicNode;
use sha2::Digest as _;
use std::collections::BTreeMap;

const VOTER_CHANGE_DOMAIN: &[u8] = b"bmux.cluster.voter-change.v2\0";
const AUTHORIZATION_LIFETIME_MS: u64 = 300_000;

fn authorization_payload(auth: &ConsensusVoterChangeAuthorization) -> Result<Vec<u8>, String> {
    if auth.protocol_version != 2 {
        return Err(
            "unsupported voter-change authorization version; upgrade all cluster peers".into(),
        );
    }
    let mut payload = VOTER_CHANGE_DOMAIN.to_vec();
    for field in [
        auth.cluster_id.as_bytes(),
        match auth.action {
            ConsensusVoterChangeAction::Add => b"add".as_slice(),
            ConsensusVoterChangeAction::Remove => b"remove".as_slice(),
        },
        auth.target_node_id.as_bytes(),
        auth.actor_node_id.as_bytes(),
        auth.membership_epoch.as_bytes(),
        &auth.current_members_digest,
        &auth.proposed_members_digest,
        &auth.expires_at_unix_ms.to_be_bytes(),
    ] {
        let length = u64::try_from(field.len())
            .map_err(|_| "voter-change authorization field is too large".to_string())?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(field);
    }
    Ok(payload)
}

/// Signs an exact transition from one committed membership epoch.
/// # Errors
/// Rejects noncanonical or oversized membership plans.
pub fn authorize_voter_change(
    cluster_id: &str,
    action: ConsensusVoterChangeAction,
    target_node_id: &str,
    identity: &crate::membership::NodeIdentity,
    membership_epoch: &str,
    current: &[ClusterMember],
    prospective: &[ClusterMember],
) -> Result<ConsensusVoterChangeAuthorization, String> {
    let mut auth = ConsensusVoterChangeAuthorization {
        protocol_version: 2,
        cluster_id: cluster_id.to_string(),
        action,
        target_node_id: target_node_id.to_string(),
        actor_node_id: identity.node_id().to_string(),
        membership_epoch: membership_epoch.to_string(),
        current_members_digest: crate::control_codec::canonical_members_digest(current)?,
        proposed_members_digest: crate::control_codec::canonical_members_digest(prospective)?,
        expires_at_unix_ms: crate::now_unix_ms().saturating_add(AUTHORIZATION_LIFETIME_MS),
        signature: Vec::new(),
    };
    auth.signature = identity.sign(&authorization_payload(&auth)?);
    Ok(auth)
}

/// Constructs the exact removal plan without introducing unsigned timestamps.
/// # Errors
/// Rejects unknown or inactive targets.
pub fn removal_plan(current: &[ClusterMember], target: &str) -> Result<Vec<ClusterMember>, String> {
    let mut proposed = current.to_vec();
    let member = proposed
        .iter_mut()
        .find(|member| member.node_id == target)
        .ok_or("removed consensus voter is not a member")?;
    if member.state != ClusterMemberState::Active
        || member.capabilities.consensus_role != ClusterConsensusRole::Voter
    {
        return Err("removed member is not an active voter".into());
    }
    member.state = ClusterMemberState::Left;
    member.updated_at_unix_ms = member
        .updated_at_unix_ms
        .checked_add(1)
        .ok_or("membership transition timestamp exhausted")?;
    Ok(proposed)
}

/// Verifies current committed authority, exact plan, freshness and issuer trust.
/// The caller must serialize verification with the actual Raft membership change.
/// # Errors
/// Rejects stale/replayed authorizations, untrusted signers and modified plans.
pub fn verify_voter_change_authorization(
    auth: &ConsensusVoterChangeAuthorization,
    expected_cluster_id: &str,
    membership_epoch: &str,
    current: &[ClusterMember],
    prospective: &[ClusterMember],
) -> Result<(), String> {
    let payload = authorization_payload(auth)?;
    let now = crate::now_unix_ms();
    if auth.cluster_id != expected_cluster_id
        || auth.membership_epoch != membership_epoch
        || auth.expires_at_unix_ms < now
        || auth.expires_at_unix_ms > now.saturating_add(AUTHORIZATION_LIFETIME_MS)
        || auth.current_members_digest != crate::control_codec::canonical_members_digest(current)?
        || auth.proposed_members_digest
            != crate::control_codec::canonical_members_digest(prospective)?
    {
        return Err(
            "voter-change authorization has a stale epoch, expired proof or modified plan".into(),
        );
    }
    let actor = current
        .iter()
        .find(|member| member.node_id == auth.actor_node_id)
        .ok_or("voter-change authorization actor is not a committed member")?;
    if actor.cluster_id != expected_cluster_id
        || actor.state != ClusterMemberState::Active
        || actor.capabilities.consensus_role != ClusterConsensusRole::Voter
    {
        return Err("voter-change authorization actor is not a current active voter".into());
    }
    // A committed member is already a trust anchor. Do not require its historical
    // credential issuer to remain active (issuer departure must not strand peers).
    crate::membership::verify_membership_credential(actor, now)?;
    crate::membership::verify_node_signature(&actor.node_id, &payload, &auth.signature)?;
    match auth.action {
        ConsensusVoterChangeAction::Add => {
            let target = prospective
                .iter()
                .find(|member| member.node_id == auth.target_node_id)
                .ok_or("membership transition omitted its target")?;
            if target.cluster_id != expected_cluster_id
                || target.state != ClusterMemberState::Active
                || target.capabilities.consensus_role != ClusterConsensusRole::Voter
            {
                return Err("membership transition target is not an active voter".into());
            }
            crate::membership::verify_membership_credential(target, now)?;
            if current
                .iter()
                .any(|member| member.node_id == target.node_id)
            {
                // An explicit current-epoch proof of an exact committed plan is
                // an acknowledgement, never permission to promote or rewrite it.
                if auth.current_members_digest != auth.proposed_members_digest {
                    return Err("existing voter acknowledgement may not modify membership".into());
                }
                validate_membership_transition(current, prospective)?;
                return Ok(());
            }
            let issuer = current
                .iter()
                .find(|member| {
                    member.node_id == target.credential_issuer_node_id
                        && member.public_key == target.credential_issuer_public_key
                        && member.cluster_id == expected_cluster_id
                        && member.state == ClusterMemberState::Active
                        && member.capabilities.consensus_role == ClusterConsensusRole::Voter
                })
                .ok_or("joining credential issuer is not a trusted current voter")?;
            crate::membership::verify_membership_credential(issuer, now)?;
            let mut exact = current.to_vec();
            exact.push(target.clone());
            if crate::control_codec::canonical_members_digest(&exact)?
                != auth.proposed_members_digest
            {
                return Err("voter addition may not modify other committed members".into());
            }
        }
        ConsensusVoterChangeAction::Remove => {
            if crate::control_codec::canonical_members_digest(&removal_plan(
                current,
                &auth.target_node_id,
            )?)? != auth.proposed_members_digest
            {
                return Err("voter removal may not modify other committed members".into());
            }
        }
    }
    validate_membership_transition(current, prospective)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoterSetPlan {
    pub voters: BTreeMap<NodeId, BasicNode>,
}

/// Builds the exact active voter set from authenticated membership records.
///
/// # Errors
///
/// Fails closed for malformed node IDs, expired/invalid credentials, or active
/// voters without a stable endpoint.
pub fn plan_active_voters_ref<'a>(
    members: impl IntoIterator<Item = &'a ClusterMember>,
) -> Result<VoterSetPlan, String> {
    let mut voters = BTreeMap::new();
    for member in members {
        if member.state != ClusterMemberState::Active
            || member.capabilities.consensus_role != ClusterConsensusRole::Voter
        {
            continue;
        }
        crate::membership::verify_membership_credential(member, crate::now_unix_ms())?;
        let node_id = member.node_id.parse::<NodeId>()?;
        let endpoint = member
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .ok_or_else(|| format!("active voter {node_id} has no advertised endpoint"))?;
        crate::membership::validate_advertised_endpoint(endpoint)?;
        if let Some((existing_id, _)) = voters
            .iter()
            .find(|(_, existing): &(&NodeId, &BasicNode)| existing.addr == endpoint)
        {
            return Err(format!(
                "active voters {existing_id} and {node_id} share advertised endpoint '{endpoint}'"
            ));
        }
        if voters.insert(node_id, BasicNode::new(endpoint)).is_some() {
            return Err(format!("duplicate active voter {node_id}"));
        }
    }
    if voters.is_empty() {
        return Err("membership transition would remove every consensus voter".to_string());
    }
    Ok(VoterSetPlan { voters })
}

/// Owned-member convenience wrapper for [`plan_active_voters_ref`].
///
/// # Errors
///
/// Returns the same validation failures as [`plan_active_voters_ref`].
pub fn plan_active_voters(
    members: impl IntoIterator<Item = ClusterMember>,
) -> Result<VoterSetPlan, String> {
    let members = members.into_iter().collect::<Vec<_>>();
    plan_active_voters_ref(&members)
}

/// Validates one exact voter-set transition.
///
/// Existing active voter endpoints are immutable within the transition; adding
/// and removing voters remains allowed.
///
/// # Errors
///
/// Returns credential, endpoint, identity, empty-set, or endpoint-change errors.
pub fn validate_membership_transition(
    current: &[ClusterMember],
    prospective: &[ClusterMember],
) -> Result<VoterSetPlan, String> {
    let current_plan = plan_active_voters_ref(current)?;
    let prospective_plan = plan_active_voters_ref(prospective)?;
    for (node_id, node) in &current_plan.voters {
        if let Some(replacement) = prospective_plan.voters.get(node_id)
            && replacement != node
        {
            return Err(format!(
                "consensus voter {node_id} endpoint cannot change during a membership transition"
            ));
        }
    }
    Ok(prospective_plan)
}

/// Reconciles a provided membership snapshot through the active local node.
///
/// This transaction primitive lets join, leave, and revoke transition Raft
/// around publication of plugin membership as required by each workflow.
///
/// # Errors
///
/// Returns registry, leader, quorum, credential, endpoint, or membership errors.
pub async fn reconcile_members(
    members: impl IntoIterator<Item = ClusterMember>,
    local_node_id: NodeId,
    nodes: &ConsensusNodeRegistry,
) -> Result<(), String> {
    let plan = plan_active_voters(members)?;
    change_voters(
        nodes
            .active(local_node_id)
            .map_err(|error| service_error(&error))?,
        plan,
    )
    .await
}

/// Safely removes one voter from the current authenticated membership plan.
///
/// The supplied node may already be absent from the voter set, making retries
/// idempotent. The transition rejects removal of the final voter and validates
/// all remaining voter credentials and endpoints before contacting `OpenRaft`.
///
/// # Errors
///
/// Returns when the node is not an active voter, when the resulting plan is
/// invalid, or when the consensus transition cannot commit.
pub async fn remove_voter(
    members: impl IntoIterator<Item = ClusterMember>,
    remove_node_id: NodeId,
    local_node_id: NodeId,
    nodes: &ConsensusNodeRegistry,
) -> Result<(), String> {
    let mut known = false;
    let prospective = members.into_iter().map(|mut member| {
        if member.node_id == remove_node_id.to_string() {
            known = true;
            if member.state == ClusterMemberState::Active
                && member.capabilities.consensus_role == ClusterConsensusRole::Voter
            {
                member.state = ClusterMemberState::Left;
            }
        }
        member
    });
    let plan = plan_active_voters(prospective)?;
    if !known {
        return Err(format!("consensus member {remove_node_id} is unknown"));
    }
    change_voters(
        nodes
            .active(local_node_id)
            .map_err(|error| service_error(&error))?,
        plan,
    )
    .await
}

/// Publishes signed membership records through deterministic idempotent
/// control commands.
///
/// # Errors
///
/// Returns control-plane leader, quorum, validation, or storage failures.
pub async fn publish_members(
    node: ConsensusNode,
    principal_id: &str,
    members: impl IntoIterator<Item = ClusterMember>,
) -> Result<(), String> {
    publish_members_impl(node, principal_id, members.into_iter().collect(), false).await
}

pub(crate) async fn publish_members_locked(
    node: ConsensusNode,
    principal_id: &str,
    members: Vec<ClusterMember>,
) -> Result<(), String> {
    publish_members_impl(node, principal_id, members, true).await
}

async fn publish_members_impl(
    node: ConsensusNode,
    principal_id: &str,
    mut members: Vec<ClusterMember>,
    locked: bool,
) -> Result<(), String> {
    members.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    for member in members {
        let command = membership_command(member, principal_id)?;
        let result = if locked {
            node.mutate_locked(command).await
        } else {
            node.mutate(command).await
        };
        result.map_err(|error| format!("replicated membership mutation failed: {error:?}"))?;
    }
    Ok(())
}

fn membership_command(
    member: ClusterMember,
    principal_id: &str,
) -> Result<bmux_cluster_plugin_api::cluster_types::ControlCommand, String> {
    Ok(bmux_cluster_plugin_api::cluster_types::ControlCommand {
        schema_version: crate::control_state::CONTROL_SCHEMA_VERSION,
        principal_id: principal_id.to_string(),
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
            value: membership_command_id(&member)?,
        },
        issued_at_unix_ms: member.updated_at_unix_ms,
        request: bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::UpsertMember {
            member,
        },
    })
}

pub(crate) fn preflight_publication(
    mut state: crate::control_state::ControlState,
    principal_id: &str,
    members: &[ClusterMember],
) -> Result<(), String> {
    let mut members = members.to_vec();
    members.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    for member in members {
        let response = state.apply(&membership_command(member, principal_id)?);
        if let bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected { error } =
            response.result
        {
            return Err(format!(
                "membership publication preflight failed: {error:?}"
            ));
        }
    }
    Ok(())
}

fn membership_command_id(member: &ClusterMember) -> Result<uuid::Uuid, String> {
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.membership-command.v1\0");
    digest.update(member.cluster_id.as_bytes());
    digest.update(member.node_id.as_bytes());
    digest.update(member.credential_serial.as_bytes());
    digest.update(format!("{:?}", member.state).as_bytes());
    let bytes: [u8; 32] = digest.finalize().into();
    Ok(uuid::Uuid::from_bytes(bytes[..16].try_into().map_err(
        |_| "membership command digest does not contain a UUID prefix".to_string(),
    )?))
}

/// Applies a previously validated voter plan.
///
/// # Errors
///
/// Returns leader, quorum, storage, or membership transition failures.
pub async fn change_voters(node: ConsensusNode, plan: VoterSetPlan) -> Result<(), String> {
    node.change_voters(plan.voters)
        .await
        .map(|_| ())
        .map_err(|error| write_error(&error))
}

fn service_error(error: &bmux_cluster_plugin_api::cluster_types::ControlServiceError) -> String {
    format!("consensus voter transition is unavailable: {error:?}")
}

fn write_error(error: &ConsensusWriteError) -> String {
    format!("consensus voter transition failed: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::{
        ClusterNegotiatedProtocol, ClusterNodeCapabilities,
    };

    fn member(id: NodeId, role: ClusterConsensusRole, state: ClusterMemberState) -> ClusterMember {
        let identity = crate::membership::NodeIdentity::new_for_test(u64::from(id.as_bytes()[31]));
        let cluster_id = "cluster:00000000-0000-0000-0000-000000000001"
            .parse::<crate::membership::ClusterId>()
            .unwrap();
        let now = crate::now_unix_ms();
        let mut member = crate::membership::issue_membership_credential(
            &identity,
            cluster_id,
            id.to_string(),
            id.public_key().unwrap().to_string(),
            ClusterNodeCapabilities {
                consensus_role: role,
                worker: true,
                ingress: false,
            },
            ClusterNegotiatedProtocol {
                wire_epoch: 1,
                peer_revision: 1,
                schema_version: 1,
                local_plugin_version: "test".to_string(),
                remote_plugin_version: "test".to_string(),
                features: Vec::new(),
            },
            now,
        )
        .unwrap();
        member.state = state;
        member.endpoint = Some(format!("tls://node-{}:7443", id.as_bytes()[31]));
        member
    }

    #[test]
    fn plans_only_active_voters_and_requires_endpoints() {
        let first = NodeId::from(1);
        let second = NodeId::from(2);
        let plan = plan_active_voters([
            member(
                first,
                ClusterConsensusRole::Voter,
                ClusterMemberState::Active,
            ),
            member(
                second,
                ClusterConsensusRole::ObserverEdge,
                ClusterMemberState::Active,
            ),
        ])
        .unwrap();
        assert_eq!(plan.voters.keys().copied().collect::<Vec<_>>(), vec![first]);

        let mut missing = member(
            first,
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        missing.endpoint = None;
        assert!(
            plan_active_voters([missing])
                .unwrap_err()
                .contains("endpoint")
        );
    }

    #[test]
    fn refuses_empty_voter_set() {
        let observer = member(
            NodeId::from(2),
            ClusterConsensusRole::ObserverEdge,
            ClusterMemberState::Active,
        );
        assert!(
            plan_active_voters([observer])
                .unwrap_err()
                .contains("every consensus voter")
        );
    }

    #[test]
    fn ignores_revoked_and_left_voters() {
        let active = NodeId::from(1);
        let plan = plan_active_voters([
            member(
                active,
                ClusterConsensusRole::Voter,
                ClusterMemberState::Active,
            ),
            member(
                NodeId::from(2),
                ClusterConsensusRole::Voter,
                ClusterMemberState::Revoked,
            ),
            member(
                NodeId::from(3),
                ClusterConsensusRole::Voter,
                ClusterMemberState::Left,
            ),
        ])
        .unwrap();
        assert_eq!(
            plan.voters.keys().copied().collect::<Vec<_>>(),
            vec![active]
        );
    }

    #[test]
    fn rejects_invalid_active_voter_credential() {
        let mut voter = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        voter.credential_signature = "00".to_string();
        assert!(
            plan_active_voters([voter])
                .unwrap_err()
                .contains("signature")
        );
    }

    fn transition_fixture() -> (
        crate::membership::NodeIdentity,
        Vec<ClusterMember>,
        Vec<ClusterMember>,
    ) {
        let issuer = crate::membership::NodeIdentity::new_for_test(1);
        let joiner = crate::membership::NodeIdentity::new_for_test(2);
        let current = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        let target = crate::membership::issue_test_member(
            &issuer,
            current.cluster_id.parse().unwrap(),
            &joiner,
            "tls://node-2:7443",
            current.capabilities.clone(),
            crate::now_unix_ms(),
        );
        (issuer, vec![current.clone()], vec![current, target])
    }

    #[test]
    fn exact_signed_add_and_remove_transitions_are_valid() {
        let (issuer, current, proposed) = transition_fixture();
        let auth = authorize_voter_change(
            &current[0].cluster_id,
            ConsensusVoterChangeAction::Add,
            &proposed[1].node_id,
            &issuer,
            "epoch-1",
            &current,
            &proposed,
        )
        .unwrap();
        verify_voter_change_authorization(
            &auth,
            &current[0].cluster_id,
            "epoch-1",
            &current,
            &proposed,
        )
        .unwrap();
        // Canonical order is independent of presentation order.
        let mut reordered = proposed.clone();
        reordered.reverse();
        verify_voter_change_authorization(
            &auth,
            &current[0].cluster_id,
            "epoch-1",
            &current,
            &reordered,
        )
        .unwrap();
        let removed = removal_plan(&proposed, &proposed[1].node_id).unwrap();
        let auth = authorize_voter_change(
            &current[0].cluster_id,
            ConsensusVoterChangeAction::Remove,
            &proposed[1].node_id,
            &issuer,
            "epoch-2",
            &proposed,
            &removed,
        )
        .unwrap();
        verify_voter_change_authorization(
            &auth,
            &current[0].cluster_id,
            "epoch-2",
            &proposed,
            &removed,
        )
        .unwrap();
    }

    #[test]
    fn authorization_rejects_prospective_signer_and_untrusted_credential_issuer() {
        let (issuer, current, proposed) = transition_fixture();
        let outsider = crate::membership::NodeIdentity::new_for_test(2);
        let mut auth = authorize_voter_change(
            &current[0].cluster_id,
            ConsensusVoterChangeAction::Add,
            &proposed[1].node_id,
            &outsider,
            "epoch-1",
            &current,
            &proposed,
        )
        .unwrap();
        assert!(
            verify_voter_change_authorization(
                &auth,
                &current[0].cluster_id,
                "epoch-1",
                &current,
                &proposed
            )
            .unwrap_err()
            .contains("committed member")
        );
        let mut forged = proposed;
        forged[1] = member(
            NodeId::from(2),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        // Even a trusted signer cannot turn a self-issued outsider into an enrolled voter.
        auth = authorize_voter_change(
            &current[0].cluster_id,
            ConsensusVoterChangeAction::Add,
            &forged[1].node_id,
            &issuer,
            "epoch-1",
            &current,
            &forged,
        )
        .unwrap();
        assert!(
            verify_voter_change_authorization(
                &auth,
                &current[0].cluster_id,
                "epoch-1",
                &current,
                &forged
            )
            .unwrap_err()
            .contains("credential issuer")
        );
    }

    #[test]
    fn authorization_binds_every_plan_field_epoch_action_and_expiry() {
        let (issuer, current, proposed) = transition_fixture();
        let auth = authorize_voter_change(
            &current[0].cluster_id,
            ConsensusVoterChangeAction::Add,
            &proposed[1].node_id,
            &issuer,
            "epoch-1",
            &current,
            &proposed,
        )
        .unwrap();
        for index in 0..7 {
            let mut changed = proposed.clone();
            match index {
                0 => changed[1].endpoint = Some("tls://changed:7443".into()),
                1 => changed[1].capabilities.worker = !changed[1].capabilities.worker,
                2 => changed[1].credential_serial.push('x'),
                3 => changed[1]
                    .negotiated_protocol
                    .features
                    .push("injected".into()),
                4 => changed[0].state = ClusterMemberState::Left,
                5 => changed[1].updated_at_unix_ms += 1,
                _ => changed.push(changed[1].clone()),
            }
            assert!(
                verify_voter_change_authorization(
                    &auth,
                    &current[0].cluster_id,
                    "epoch-1",
                    &current,
                    &changed
                )
                .is_err()
            );
        }
        assert!(
            verify_voter_change_authorization(
                &auth,
                &current[0].cluster_id,
                "epoch-2",
                &current,
                &proposed
            )
            .is_err()
        );
        for index in 0..6 {
            let mut changed = auth.clone();
            match index {
                0 => changed.action = ConsensusVoterChangeAction::Remove,
                1 => changed.target_node_id = current[0].node_id.clone(),
                2 => changed.expires_at_unix_ms = crate::now_unix_ms().saturating_sub(1),
                3 => changed.protocol_version = 1,
                4 => changed.protocol_version = 0,
                _ => changed.signature = vec![0; 64],
            }
            assert!(
                verify_voter_change_authorization(
                    &changed,
                    &current[0].cluster_id,
                    "epoch-1",
                    &current,
                    &proposed
                )
                .is_err()
            );
        }
    }

    #[test]
    fn membership_command_identity_is_stable_and_transition_specific() {
        let active = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        assert_eq!(
            membership_command_id(&active).unwrap(),
            membership_command_id(&active).unwrap()
        );
        let mut left = active.clone();
        left.state = ClusterMemberState::Left;
        assert_ne!(
            membership_command_id(&active).unwrap(),
            membership_command_id(&left).unwrap()
        );
        let mut rotated = active.clone();
        rotated.credential_serial = "rotated".to_string();
        assert_ne!(
            membership_command_id(&active).unwrap(),
            membership_command_id(&rotated).unwrap()
        );
    }

    #[test]
    fn active_voter_plan_rejects_duplicate_and_noncanonical_endpoints() {
        let first = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        let mut duplicate = member(
            NodeId::from(2),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        duplicate.endpoint.clone_from(&first.endpoint);
        assert!(
            plan_active_voters([first.clone(), duplicate])
                .unwrap_err()
                .contains("share advertised endpoint")
        );

        let mut wrong_identity = first.clone();
        wrong_identity.node_id = "node:not-a-key".to_string();
        assert!(plan_active_voters([wrong_identity]).is_err());

        let mut local = first.clone();
        local.endpoint = Some("local".to_string());
        assert!(
            plan_active_voters([local])
                .unwrap_err()
                .contains("node-local")
        );
        let mut malformed = first;
        malformed.endpoint = Some("tls://".to_string());
        assert!(
            plan_active_voters([malformed])
                .unwrap_err()
                .contains("authority")
        );
    }

    #[test]
    fn transition_allows_add_remove_and_rejects_endpoint_rewrite() {
        let first = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        let second = member(
            NodeId::from(2),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        validate_membership_transition(
            std::slice::from_ref(&first),
            &[first.clone(), second.clone()],
        )
        .unwrap();
        let mut removed = first.clone();
        removed.state = ClusterMemberState::Left;
        validate_membership_transition(&[first, second.clone()], &[removed, second.clone()])
            .unwrap();

        let mut rewritten = second.clone();
        rewritten.endpoint = Some("tls://different:7443".to_string());
        assert!(
            validate_membership_transition(std::slice::from_ref(&second), &[rewritten])
                .unwrap_err()
                .contains("cannot change")
        );
    }
}
