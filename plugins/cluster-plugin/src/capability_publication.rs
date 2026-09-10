//! Signed supported-capability evidence; never changes active negotiation.
use crate::membership::NodeId;
use bmux_cluster_plugin_api::{
    cluster_capability_types::CapabilityReport,
    cluster_types::{ClusterMember, ClusterMemberState},
};
use openraft::{BasicNode, StoredMembership};

/// Canonical domain-separated statement, excluding the detached signature.
/// # Errors
/// Rejects unknown representations, invalid ranges and noncanonical/unbounded fields.
pub fn signing_payload(report: &CapabilityReport) -> Result<Vec<u8>, String> {
    if report.report_version != 1
        || report.command_id.value.is_nil()
        || report.schema_min == 0
        || report.schema_min > report.schema_max
        || report.cluster_id.is_empty()
        || report.cluster_id.len() > 128
        || report.node_id.is_empty()
        || report.node_id.len() > 128
        || report.credential_serial.is_empty()
        || report.credential_serial.len() > 1024
        || report.membership_leader_id.is_empty()
        || report.membership_leader_id.len() > 128
        || report.features.len() > 128
        || report
            .features
            .iter()
            .any(|f| f.is_empty() || f.len() > 128)
        || report.features.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("invalid capability report representation or bounds".into());
    }
    let mut w = crate::control_codec::Writer::default();
    w.raw(b"bmux.cluster.supported-capabilities.v1\0");
    w.u16(report.report_version);
    w.string(&report.cluster_id);
    w.string(&report.node_id);
    w.string(&report.credential_serial);
    w.u64(report.membership_term);
    w.string(&report.membership_leader_id);
    w.u64(report.membership_log_index);
    w.u64(report.expected_report_revision);
    w.uuid(report.command_id.value);
    w.u32(report.schema_min);
    w.u32(report.schema_max);
    w.u32(u32::try_from(report.features.len()).map_err(|e| e.to_string())?);
    for feature in &report.features {
        w.string(feature);
    }
    Ok(w.into_bytes())
}

/// Verifies report proof against current committed authority, not endpoint access.
/// The authoritative caller supplies verification time; this function performs no I/O.
/// # Errors
/// Rejects stale membership, revoked/expired credentials, mismatched identity or proof.
pub fn verify_report(
    report: &CapabilityReport,
    member: &ClusterMember,
    membership: &StoredMembership<NodeId, BasicNode>,
    cluster_id: &str,
    verified_at_unix_ms: u64,
) -> Result<(), String> {
    let payload = signing_payload(report)?;
    let position = membership
        .log_id()
        .as_ref()
        .ok_or("missing committed membership")?;
    let node: NodeId = report
        .node_id
        .parse()
        .map_err(|_| "invalid report node identity")?;
    if report.signature.len() != 64
        || report.cluster_id != cluster_id
        || member.cluster_id != cluster_id
        || report.node_id != member.node_id
        || report.credential_serial != member.credential_serial
        || member.state != ClusterMemberState::Active
        || membership.membership().get_node(&node).is_none()
        || report.membership_term != position.leader_id.term
        || report.membership_leader_id != position.leader_id.node_id.to_string()
        || report.membership_log_index != position.index
    {
        return Err("capability report authority mismatch".into());
    }
    crate::membership::verify_membership_credential(member, verified_at_unix_ms)?;
    let key = member
        .public_key
        .parse::<iroh::PublicKey>()
        .map_err(|e| e.to_string())?;
    let signature =
        iroh::Signature::try_from(report.signature.as_slice()).map_err(|e| e.to_string())?;
    key.verify(&payload, &signature)
        .map_err(|_| "invalid capability report signature".into())
}

/// Validates a complete bridge proof before proposing a new representation.
/// This is evidence of decoder support, not feature activation or report storage.
/// # Errors
/// Rejects incomplete/duplicate evidence, missing active authority, and incompatible recipients.
pub fn verify_bridge(
    reports: &[CapabilityReport],
    members: &std::collections::BTreeMap<String, ClusterMember>,
    membership: &StoredMembership<NodeId, BasicNode>,
    cluster_id: &str,
    verified_at_unix_ms: u64,
) -> Result<(), String> {
    const MAX_RECIPIENTS: usize = 128;
    if reports.is_empty() || reports.len() > MAX_RECIPIENTS {
        return Err("invalid bridge recipient count".into());
    }
    let committed = membership.membership();
    if committed.voter_ids().next().is_none()
        || committed.nodes().take(MAX_RECIPIENTS + 1).count() != reports.len()
        || members
            .values()
            .filter(|member| member.state == ClusterMemberState::Active)
            .take(MAX_RECIPIENTS + 1)
            .count()
            != reports.len()
    {
        return Err("bridge evidence must cover exact committed active membership".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for report in reports {
        if !seen.insert(&report.node_id) {
            return Err("duplicate bridge recipient".into());
        }
        let member = members
            .get(&report.node_id)
            .ok_or("bridge member missing")?;
        verify_report(report, member, membership, cluster_id, verified_at_unix_ms)?;
        // The bridge's publication encoding is separately versioned from the
        // eventual feature floor; advertising refresh support is insufficient.
        if !report
            .features
            .iter()
            .any(|feature| feature == "capability-publication-v1")
        {
            return Err("recipient does not support capability publication representation".into());
        }
    }
    Ok(())
}

/// A publication batch carries complete recipient evidence. It does not activate
/// the reported features. Verification time must be assigned by the leader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationCommand {
    pub reports: Vec<CapabilityReport>,
    pub verified_at_unix_ms: u64,
}

impl PublicationCommand {
    /// Encodes the explicit bridge command format, independently of Rust layout.
    /// # Errors
    /// Rejects oversized batches, malformed reports and duplicate/noncanonical order.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.reports.is_empty()
            || self.reports.len() > 128
            || self
                .reports
                .windows(2)
                .any(|pair| pair[0].node_id >= pair[1].node_id)
        {
            return Err("invalid publication report count or order".into());
        }
        let mut w = crate::control_codec::Writer::default();
        w.raw(b"BMCAP001");
        w.u16(1);
        w.u64(self.verified_at_unix_ms);
        w.u16(u16::try_from(self.reports.len()).map_err(|e| e.to_string())?);
        for report in &self.reports {
            if report.signature.len() != 64 {
                return Err("invalid report signature length".into());
            }
            w.bytes(&signing_payload(report)?);
            w.bytes(&report.signature);
        }
        Ok(w.into_bytes())
    }

    /// Decodes the bounded command and validates its canonical representation.
    /// # Errors
    /// Rejects unknown versions, malformed reports, trailing data and oversized input.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        use crate::control_codec::{CodecError, Reader};
        // Each of 128 reports has at most 128 bounded feature names.
        if bytes.len() > 3 * 1024 * 1024 {
            return Err("publication command exceeds limit".into());
        }
        let parse = || -> Result<Self, CodecError> {
            let mut r = Reader::new(bytes);
            if r.take(8)? != b"BMCAP001" {
                return Err(CodecError::InvalidMagic);
            }
            let version = r.u16()?;
            if version != 1 {
                return Err(CodecError::UnsupportedSchema(version));
            }
            let verified_at_unix_ms = r.u64()?;
            let count = r.u16()?;
            if count == 0 || count > 128 {
                return Err(CodecError::LimitExceeded("publication reports"));
            }
            let mut reports = Vec::with_capacity(usize::from(count));
            for _ in 0..count {
                let payload = r.bytes()?;
                let mut report = decode_report_payload(&payload)?;
                report.signature = r.bytes()?;
                reports.push(report);
            }
            r.finish()?;
            Ok(Self {
                reports,
                verified_at_unix_ms,
            })
        };
        let command = parse().map_err(|e| e.to_string())?;
        command.encode()?;
        Ok(command)
    }
}

fn decode_report_payload(
    bytes: &[u8],
) -> Result<CapabilityReport, crate::control_codec::CodecError> {
    use crate::control_codec::{CodecError, Reader};
    let mut r = Reader::new(bytes);
    let domain = b"bmux.cluster.supported-capabilities.v1\0";
    if r.take(domain.len())? != domain {
        return Err(CodecError::InvalidMagic);
    }
    let report_version = r.u16()?;
    let cluster_id = r.string()?;
    let node_id = r.string()?;
    let credential_serial = r.string()?;
    let membership_term = r.u64()?;
    let membership_leader_id = r.string()?;
    let membership_log_index = r.u64()?;
    let expected_report_revision = r.u64()?;
    let command_id = bmux_cluster_plugin_api::cluster_types::CommandId { value: r.uuid()? };
    let schema_min = r.u32()?;
    let schema_max = r.u32()?;
    let count = r.u32()?;
    if count > 128 {
        return Err(CodecError::LimitExceeded("report features"));
    }
    let mut features = Vec::new();
    for _ in 0..count {
        features.push(r.string()?);
    }
    r.finish()?;
    Ok(CapabilityReport {
        report_version,
        cluster_id,
        node_id,
        credential_serial,
        membership_term,
        membership_leader_id,
        membership_log_index,
        expected_report_revision,
        command_id,
        schema_min,
        schema_max,
        features,
        signature: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::{ClusterId, NodeIdentity, initializer_capabilities, issue_test_member};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn report_binds_current_authority_and_canonical_capabilities() {
        let identity = NodeIdentity::new_for_test(93);
        let node = *identity.node_id();
        let cluster: ClusterId = "cluster:00000000-0000-0000-0000-000000000093"
            .parse()
            .unwrap();
        let member = issue_test_member(
            &identity,
            cluster,
            &identity,
            "tls://127.0.0.1:49993",
            initializer_capabilities(),
            42,
        );
        let membership = StoredMembership::new(
            Some(openraft::LogId::new(
                openraft::CommittedLeaderId::new(1, node),
                2,
            )),
            openraft::Membership::new(
                vec![BTreeSet::from([node])],
                BTreeMap::from([(node, BasicNode::new("node"))]),
            ),
        );
        let mut report = CapabilityReport {
            report_version: 1,
            cluster_id: cluster.to_string(),
            node_id: node.to_string(),
            credential_serial: member.credential_serial.clone(),
            membership_term: 1,
            membership_leader_id: node.to_string(),
            membership_log_index: 2,
            expected_report_revision: 0,
            command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
                value: uuid::Uuid::new_v4(),
            },
            schema_min: 1,
            schema_max: 4,
            features: vec!["protocol-refresh-v1".into()],
            signature: Vec::new(),
        };
        report.signature = identity.sign(&signing_payload(&report).unwrap());
        verify_report(&report, &member, &membership, &cluster.to_string(), 43).unwrap();
        let members = BTreeMap::from([(member.node_id.clone(), member.clone())]);
        assert!(
            verify_bridge(
                std::slice::from_ref(&report),
                &members,
                &membership,
                &cluster.to_string(),
                43
            )
            .is_err()
        );
        let mut bridge = report.clone();
        bridge
            .features
            .insert(0, "capability-publication-v1".into());
        bridge.signature = identity.sign(&signing_payload(&bridge).unwrap());
        verify_bridge(
            std::slice::from_ref(&bridge),
            &members,
            &membership,
            &cluster.to_string(),
            43,
        )
        .unwrap();
        assert!(verify_bridge(&[], &members, &membership, &cluster.to_string(), 43).is_err());
        assert!(
            verify_bridge(
                &[bridge.clone(), bridge.clone()],
                &members,
                &membership,
                &cluster.to_string(),
                43
            )
            .is_err()
        );
        let learner = NodeId::from(94);
        let expanded = StoredMembership::new(
            *membership.log_id(),
            openraft::Membership::new(
                vec![BTreeSet::from([node])],
                BTreeMap::from([
                    (node, BasicNode::new("node")),
                    (learner, BasicNode::new("learner")),
                ]),
            ),
        );
        assert!(verify_bridge(&[bridge], &members, &expanded, &cluster.to_string(), 43).is_err());
        assert_publication_codec(&report);
        assert_report_mutations_rejected(report, &member, &membership, cluster, &identity);
    }

    fn assert_publication_codec(report: &CapabilityReport) {
        let command = PublicationCommand {
            reports: vec![report.clone()],
            verified_at_unix_ms: 43,
        };
        let bytes = command.encode().unwrap();
        assert_eq!(PublicationCommand::decode(&bytes).unwrap(), command);
        for end in 0..bytes.len() {
            assert!(PublicationCommand::decode(&bytes[..end]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(PublicationCommand::decode(&trailing).is_err());
        let mut unknown = bytes;
        unknown[8] = 255;
        assert!(PublicationCommand::decode(&unknown).is_err());
        let duplicate = PublicationCommand {
            reports: vec![report.clone(), report.clone()],
            verified_at_unix_ms: 43,
        };
        assert!(duplicate.encode().is_err());
    }

    fn assert_report_mutations_rejected(
        report: CapabilityReport,
        member: &ClusterMember,
        membership: &StoredMembership<NodeId, BasicNode>,
        cluster: ClusterId,
        identity: &NodeIdentity,
    ) {
        let mut changed = report.clone();
        changed.schema_max = 5;
        assert!(verify_report(&changed, member, membership, &cluster.to_string(), 43).is_err());
        changed = report.clone();
        changed.membership_log_index += 1;
        changed.signature = identity.sign(&signing_payload(&changed).unwrap());
        assert!(verify_report(&changed, member, membership, &cluster.to_string(), 43).is_err());
        changed = report.clone();
        changed.features.push(changed.features[0].clone());
        assert!(signing_payload(&changed).is_err());
        changed = report.clone();
        changed.report_version = 2;
        assert!(signing_payload(&changed).is_err());
        changed = report;
        changed.credential_serial.push('x');
        changed.signature = identity.sign(&signing_payload(&changed).unwrap());
        assert!(verify_report(&changed, member, membership, &cluster.to_string(), 43).is_err());
    }
}
