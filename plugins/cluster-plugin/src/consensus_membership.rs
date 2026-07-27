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

const VOTER_CHANGE_DOMAIN: &[u8] = b"bmux.cluster.voter-change.v1\0";

fn authorization_payload(
    cluster_id: &str,
    action: ConsensusVoterChangeAction,
    target_node_id: &str,
    actor_node_id: &str,
) -> Result<Vec<u8>, String> {
    let action = match action {
        ConsensusVoterChangeAction::Add => b"add".as_slice(),
        ConsensusVoterChangeAction::Remove => b"remove".as_slice(),
    };
    let mut payload = VOTER_CHANGE_DOMAIN.to_vec();
    for field in [
        cluster_id.as_bytes(),
        action,
        target_node_id.as_bytes(),
        actor_node_id.as_bytes(),
    ] {
        let length = u64::try_from(field.len())
            .map_err(|_| "voter-change authorization field is too large".to_string())?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(field);
    }
    Ok(payload)
}

/// Signs one voter-set transition authorization with the local node identity.
///
/// # Errors
///
/// Returns an error if canonical authorization encoding fails.
pub fn authorize_voter_change(
    cluster_id: &str,
    action: ConsensusVoterChangeAction,
    target_node_id: &str,
    identity: &crate::membership::NodeIdentity,
) -> Result<ConsensusVoterChangeAuthorization, String> {
    let actor_node_id = identity.node_id().to_string();
    let signature = identity.sign(&authorization_payload(
        cluster_id,
        action,
        target_node_id,
        &actor_node_id,
    )?);
    Ok(ConsensusVoterChangeAuthorization {
        cluster_id: cluster_id.to_string(),
        action,
        target_node_id: target_node_id.to_string(),
        actor_node_id,
        signature,
    })
}

/// Verifies that a voter-change authorization is signed by an active voter.
///
/// # Errors
///
/// Returns cluster, action, target, actor, credential, or signature failures.
pub fn verify_voter_change_authorization(
    authorization: &ConsensusVoterChangeAuthorization,
    expected_cluster_id: &str,
    expected_action: ConsensusVoterChangeAction,
    expected_target_node_id: NodeId,
    members: &[ClusterMember],
) -> Result<(), String> {
    if authorization.cluster_id != expected_cluster_id {
        return Err("voter-change authorization belongs to another cluster".to_string());
    }
    if authorization.action != expected_action {
        return Err("voter-change authorization has the wrong action".to_string());
    }
    if authorization.target_node_id != expected_target_node_id.to_string() {
        return Err("voter-change authorization has the wrong target".to_string());
    }
    let actor = members
        .iter()
        .find(|member| member.node_id == authorization.actor_node_id)
        .ok_or_else(|| "voter-change authorization actor is not a member".to_string())?;
    if actor.state != ClusterMemberState::Active
        || actor.capabilities.consensus_role != ClusterConsensusRole::Voter
    {
        return Err("voter-change authorization actor is not an active voter".to_string());
    }
    crate::membership::verify_membership_credential(actor, crate::now_unix_ms())?;
    crate::membership::verify_node_signature(
        &actor.node_id,
        &authorization_payload(
            &authorization.cluster_id,
            authorization.action,
            &authorization.target_node_id,
            &authorization.actor_node_id,
        )?,
        &authorization.signature,
    )
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
    let mut members = members.into_iter().collect::<Vec<_>>();
    members.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    for member in members {
        let command_id = membership_command_id(&member)?;
        let command = bmux_cluster_plugin_api::cluster_types::ControlCommand {
            schema_version: crate::control_state::CONTROL_SCHEMA_VERSION,
            principal_id: principal_id.to_string(),
            command_id: bmux_cluster_plugin_api::cluster_types::CommandId { value: command_id },
            issued_at_unix_ms: member.updated_at_unix_ms,
            request: bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::UpsertMember {
                member,
            },
        };
        node.mutate(command)
            .await
            .map_err(|error| format!("replicated membership mutation failed: {error:?}"))?;
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

    #[test]
    fn voter_change_authorization_is_target_and_action_bound() {
        let current = member(
            NodeId::from(1),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        let target = member(
            NodeId::from(2),
            ClusterConsensusRole::Voter,
            ClusterMemberState::Active,
        );
        let actor_identity = crate::membership::NodeIdentity::new_for_test(1);
        let authorization = authorize_voter_change(
            &current.cluster_id,
            ConsensusVoterChangeAction::Add,
            &target.node_id,
            &actor_identity,
        )
        .unwrap();
        verify_voter_change_authorization(
            &authorization,
            &current.cluster_id,
            ConsensusVoterChangeAction::Add,
            target.node_id.parse().unwrap(),
            std::slice::from_ref(&current),
        )
        .unwrap();

        let mut tampered = authorization;
        tampered.target_node_id = current.node_id.clone();
        assert!(
            verify_voter_change_authorization(
                &tampered,
                &current.cluster_id,
                ConsensusVoterChangeAction::Add,
                target.node_id.parse().unwrap(),
                std::slice::from_ref(&current),
            )
            .is_err()
        );
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
