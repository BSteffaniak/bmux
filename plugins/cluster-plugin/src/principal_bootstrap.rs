//! Principal bootstrap proof validation. This does not commit bootstrap state.

use bmux_cluster_plugin_api::cluster_principal_bootstrap_types::{
    BootstrapProof, BootstrapStatement,
};
use bmux_cluster_plugin_api::cluster_types::{ClusterMember, ClusterMemberState};
use openraft::{BasicNode, StoredMembership};
use std::collections::{BTreeMap, BTreeSet};

use crate::membership::NodeId;

const DOMAIN: &[u8] = b"bmux.cluster.principal-bootstrap.v1\0";
const MAX_APPROVALS: usize = 128;

/// Canonical, domain-separated bytes signed by the principal and each voter.
///
/// # Errors
/// Rejects unsupported versions, invalid identities and noncanonical keys.
pub fn signing_payload(statement: &BootstrapStatement) -> Result<Vec<u8>, String> {
    if statement.schema_version != 1
        || statement.cluster_id.is_empty()
        || statement.cluster_id.len() > 256
        || statement.principal_id.is_nil()
        || statement.command_id.value.is_nil()
    {
        return Err("invalid bootstrap statement identity or version".into());
    }
    let key: iroh::PublicKey = statement
        .principal_public_key
        .parse()
        .map_err(|_| "invalid principal key")?;
    if key.to_string() != statement.principal_public_key {
        return Err("noncanonical principal key".into());
    }
    let leader: NodeId = statement.membership_leader_id.parse()?;
    if leader.to_string() != statement.membership_leader_id {
        return Err("noncanonical membership leader".into());
    }
    let mut bytes = DOMAIN.to_vec();
    bytes.extend_from_slice(&statement.schema_version.to_be_bytes());
    let length = u16::try_from(statement.cluster_id.len()).map_err(|_| "cluster id too large")?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(statement.cluster_id.as_bytes());
    bytes.extend_from_slice(&statement.membership_term.to_be_bytes());
    bytes.extend_from_slice(leader.as_bytes());
    bytes.extend_from_slice(&statement.membership_log_index.to_be_bytes());
    bytes.extend_from_slice(statement.principal_id.as_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(statement.command_id.value.as_bytes());
    Ok(bytes)
}

/// Verifies proof against caller-supplied authoritative committed membership.
///
/// Callers must recheck this membership and unused-bootstrap precondition at
/// commit; successful validation is neither authorization for other operations
/// nor an acknowledgment of durable bootstrap.
///
/// # Errors
/// Rejects stale membership, bad proofs, duplicate signers and missing majorities.
pub fn verify_proof(
    proof: &BootstrapProof,
    cluster_id: &str,
    membership: &StoredMembership<NodeId, BasicNode>,
    members: &BTreeMap<String, ClusterMember>,
    now_unix_ms: u64,
) -> Result<(), String> {
    let statement = &proof.statement;
    let log = membership
        .log_id()
        .as_ref()
        .ok_or("bootstrap requires committed membership")?;
    if statement.cluster_id != cluster_id
        || statement.membership_term != log.leader_id.term
        || statement.membership_leader_id != log.leader_id.node_id.to_string()
        || statement.membership_log_index != log.index
    {
        return Err("bootstrap membership or cluster mismatch".into());
    }
    if proof.voter_approvals.len() > MAX_APPROVALS {
        return Err("too many bootstrap approvals".into());
    }
    let payload = signing_payload(statement)?;
    let principal: iroh::PublicKey = statement
        .principal_public_key
        .parse()
        .map_err(|_| "invalid principal key")?;
    let signature_bytes: &[u8; 64] = proof
        .principal_signature
        .as_slice()
        .try_into()
        .map_err(|_| "invalid principal signature")?;
    let signature = iroh::Signature::from_bytes(signature_bytes);
    principal
        .verify(&payload, &signature)
        .map_err(|_| "principal proof failed")?;
    let configs = membership.membership().get_joint_config();
    if configs.is_empty()
        || configs.len() > 2
        || configs
            .iter()
            .any(|config| config.is_empty() || config.len() > MAX_APPROVALS)
    {
        return Err("bootstrap requires one or two nonempty bounded voter configurations".into());
    }
    let mut approved = BTreeSet::new();
    for approval in &proof.voter_approvals {
        let node: NodeId = approval.node_id.parse()?;
        if !configs.iter().any(|config| config.contains(&node)) || !approved.insert(node) {
            return Err("bootstrap signer is duplicate or not a committed voter".into());
        }
        let member = members
            .get(&approval.node_id)
            .ok_or("bootstrap signer has no credential")?;
        if member.cluster_id != cluster_id {
            return Err("bootstrap signer credential belongs to another cluster".into());
        }
        if member.node_id != approval.node_id || member.state != ClusterMemberState::Active {
            return Err("bootstrap signer is inactive or mismatched".into());
        }
        crate::membership::verify_membership_credential(member, now_unix_ms)?;
        crate::membership::verify_node_signature(&approval.node_id, &payload, &approval.signature)?;
    }
    if configs
        .iter()
        .any(|config| config.intersection(&approved).count() <= config.len() / 2)
    {
        return Err("bootstrap requires a majority of every committed voter configuration".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::CommandId;
    use openraft::{CommittedLeaderId, LogId, Membership};

    fn statement() -> BootstrapStatement {
        BootstrapStatement {
            schema_version: 1,
            cluster_id: "cluster:test".into(),
            membership_term: 3,
            membership_leader_id: NodeId::from(1).to_string(),
            membership_log_index: 7,
            principal_id: uuid::Uuid::from_u128(8),
            principal_public_key: iroh::SecretKey::from_bytes(&[9; 32]).public().to_string(),
            command_id: CommandId {
                value: uuid::Uuid::from_u128(10),
            },
        }
    }

    #[test]
    fn canonical_payload_binds_every_statement_field() {
        let original = statement();
        let bytes = signing_payload(&original).unwrap();
        assert!(bytes.starts_with(DOMAIN));
        for field in 0..7 {
            let mut changed = original.clone();
            match field {
                0 => changed.cluster_id.push('x'),
                1 => changed.membership_term += 1,
                2 => changed.membership_leader_id = NodeId::from(2).to_string(),
                3 => changed.membership_log_index += 1,
                4 => changed.principal_id = uuid::Uuid::from_u128(11),
                5 => {
                    changed.principal_public_key =
                        iroh::SecretKey::from_bytes(&[12; 32]).public().to_string();
                }
                _ => changed.command_id.value = uuid::Uuid::from_u128(13),
            }
            assert_ne!(bytes, signing_payload(&changed).unwrap());
        }
        let mut invalid = original;
        invalid.schema_version = 2;
        assert!(signing_payload(&invalid).is_err());
    }

    #[test]
    fn joint_majorities_require_valid_same_cluster_credentials() {
        use crate::membership::{ClusterId, NodeIdentity, issue_membership_credential};
        use bmux_cluster_plugin_api::cluster_principal_bootstrap_types::VoterApproval;
        use bmux_cluster_plugin_api::cluster_types::{
            ClusterConsensusRole, ClusterNegotiatedProtocol, ClusterNodeCapabilities,
        };

        let cluster: ClusterId = "cluster:00000000-0000-0000-0000-000000000001"
            .parse()
            .unwrap();
        let identities: Vec<_> = (1..=3).map(NodeIdentity::new_for_test).collect();
        let ids: Vec<_> = identities
            .iter()
            .map(|identity| *identity.node_id())
            .collect();
        let configs = vec![
            BTreeSet::from([ids[0], ids[1]]),
            BTreeSet::from([ids[1], ids[2]]),
        ];
        let membership = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(3, ids[0]), 7)),
            Membership::<NodeId, BasicNode>::new(configs, None),
        );
        let mut statement = statement();
        statement.cluster_id = cluster.to_string();
        statement.membership_leader_id = ids[0].to_string();
        let payload = signing_payload(&statement).unwrap();
        let mut members = BTreeMap::new();
        for identity in &identities {
            let member = issue_membership_credential(
                identity,
                cluster,
                identity.node_id().to_string(),
                identity.public_key().to_string(),
                ClusterNodeCapabilities {
                    consensus_role: ClusterConsensusRole::Voter,
                    worker: true,
                    ingress: true,
                },
                ClusterNegotiatedProtocol {
                    wire_epoch: 1,
                    peer_revision: 1,
                    schema_version: 1,
                    local_plugin_version: "test".into(),
                    remote_plugin_version: "test".into(),
                    features: Vec::new(),
                },
                100,
            )
            .unwrap();
            members.insert(member.node_id.clone(), member);
        }
        let mut proof = BootstrapProof {
            statement,
            principal_signature: iroh::SecretKey::from_bytes(&[9; 32])
                .sign(&payload)
                .to_bytes()
                .to_vec(),
            voter_approvals: identities
                .iter()
                .map(|identity| VoterApproval {
                    node_id: identity.node_id().to_string(),
                    signature: identity.sign(&payload),
                })
                .collect(),
        };
        assert_eq!(
            verify_proof(&proof, &cluster.to_string(), &membership, &members, 100),
            Ok(())
        );
        let last = proof.voter_approvals.pop().unwrap();
        assert!(
            verify_proof(&proof, &cluster.to_string(), &membership, &members, 100)
                .unwrap_err()
                .contains("majority")
        );
        proof.voter_approvals.push(last);
        proof.voter_approvals.push(proof.voter_approvals[0].clone());
        assert!(
            verify_proof(&proof, &cluster.to_string(), &membership, &members, 100)
                .unwrap_err()
                .contains("duplicate")
        );
        proof.voter_approvals.pop();
        members.get_mut(&ids[0].to_string()).unwrap().cluster_id = "cluster:other".into();
        assert!(
            verify_proof(&proof, &cluster.to_string(), &membership, &members, 100)
                .unwrap_err()
                .contains("another cluster")
        );
    }

    #[test]
    fn principal_proof_does_not_replace_voter_majority() {
        let statement = statement();
        let key = iroh::SecretKey::from_bytes(&[9; 32]);
        let membership = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(3, NodeId::from(1)), 7)),
            Membership::<NodeId, BasicNode>::new(vec![BTreeSet::from([NodeId::from(1)])], None),
        );
        let mut proof = BootstrapProof {
            principal_signature: key
                .sign(&signing_payload(&statement).unwrap())
                .to_bytes()
                .to_vec(),
            statement,
            voter_approvals: Vec::new(),
        };
        assert!(
            verify_proof(&proof, "cluster:test", &membership, &BTreeMap::new(), 0)
                .unwrap_err()
                .contains("majority")
        );
        proof.principal_signature[0] ^= 1;
        assert!(
            verify_proof(&proof, "cluster:test", &membership, &BTreeMap::new(), 0)
                .unwrap_err()
                .contains("principal proof")
        );
        proof.statement.membership_log_index += 1;
        assert!(
            verify_proof(&proof, "cluster:test", &membership, &BTreeMap::new(), 0)
                .unwrap_err()
                .contains("mismatch")
        );
    }
}
