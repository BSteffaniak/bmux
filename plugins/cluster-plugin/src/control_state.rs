use crate::control_codec::{FeatureActivationCommand, request_fingerprint};
#[path = "control_state_codec.rs"]
mod control_state_codec;
use bmux_cluster_plugin_api::cluster_types::{
    ClusterMember, ClusterMemberState, ControlCommand, ControlCommandError, ControlCommandRequest,
    ControlCommandResult, ControlReadConsistency, ControlResourceKind, ControlResponse,
    ControlStateView, ControlWorkflowStatus, LogicalPaneRecord, LogicalTabRecord, PaneAvailability,
    PendingWorkflow, WorkspaceId, WorkspaceRecord,
};
use control_state_codec::{decode_snapshot, encode_snapshot};
use sha2::Digest as _;
use std::collections::{BTreeMap, BTreeSet};

pub const CONTROL_SCHEMA_VERSION: u16 = 1;
pub const CONTROL_CODEC_VERSION: u16 = 1;
const SNAPSHOT_FORMAT_VERSION: u16 = 3;
const PREVIOUS_SNAPSHOT_FORMAT_VERSION: u16 = 2;
const LEGACY_SNAPSHOT_FORMAT_VERSION: u16 = 1;
const SNAPSHOT_MAGIC: &[u8; 8] = b"BMSTA003";
const PREVIOUS_SNAPSHOT_MAGIC: &[u8; 8] = b"BMSTA002";
const LEGACY_SNAPSHOT_MAGIC: &[u8; 8] = b"BMSTA001";
const MAX_SNAPSHOT_ITEMS: usize = 1_000_000;
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateCodecError {
    Truncated,
    InvalidMagic,
    UnsupportedSnapshotFormat(u16),
    UnsupportedCodec(u16),
    UnsupportedSchema(u16),
    InvalidUtf8,
    InvalidBoolean(u8),
    LimitExceeded(&'static str),
    TrailingBytes,
    InvalidState(&'static str),
}

impl std::fmt::Display for StateCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("control snapshot is truncated"),
            Self::InvalidMagic => formatter.write_str("control snapshot magic is invalid"),
            Self::UnsupportedSnapshotFormat(version) => {
                write!(formatter, "unsupported control snapshot format {version}")
            }
            Self::UnsupportedCodec(version) => {
                write!(formatter, "unsupported control codec version {version}")
            }
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported control snapshot schema {version}")
            }
            Self::InvalidUtf8 => formatter.write_str("control snapshot contains invalid UTF-8"),
            Self::InvalidBoolean(value) => {
                write!(
                    formatter,
                    "control snapshot contains invalid boolean {value}"
                )
            }
            Self::LimitExceeded(name) => write!(formatter, "control snapshot {name} exceeds limit"),
            Self::TrailingBytes => formatter.write_str("control snapshot has trailing bytes"),
            Self::InvalidState(reason) => {
                write!(formatter, "control snapshot state is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for StateCodecError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DedupKey {
    principal_id: String,
    command_id: uuid::Uuid,
}

impl Ord for DedupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.principal_id, self.command_id.as_bytes())
            .cmp(&(&other.principal_id, other.command_id.as_bytes()))
    }
}

impl PartialOrd for DedupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DedupRecord {
    fingerprint: [u8; 32],
    issued_at_unix_ms: u64,
    command: ControlCommand,
    response: ControlResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeatureDedupRecord {
    fingerprint: [u8; 32],
    issued_at_unix_ms: u64,
    command: FeatureActivationCommand,
    response: ControlResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalBootstrapRecord {
    pub principal_id: uuid::Uuid,
    pub public_key: String,
    pub command_id: uuid::Uuid,
    pub statement_fingerprint: [u8; 32],
    pub committed_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlState {
    pub schema_version: u16,
    pub cluster_id: String,
    pub revision: u64,
    pub read_schema_floor: u16,
    pub write_schema_floor: u16,
    pub activated_features: BTreeSet<String>,
    pub members: BTreeMap<String, ClusterMember>,
    pub workspaces: BTreeMap<uuid::Uuid, WorkspaceRecord>,
    pub tabs: BTreeMap<uuid::Uuid, LogicalTabRecord>,
    pub panes: BTreeMap<uuid::Uuid, LogicalPaneRecord>,
    /// Permanent bootstrap consumption and retry outcome; never pruned with ordinary dedup.
    pub principal_bootstrap: Option<PrincipalBootstrapRecord>,
    // Successful refresh outcomes are bounded and persisted with the member update.
    // Bounded committed publication batches retain report revisions and retry outcomes.
    publication_history: Vec<(crate::capability_publication::PublicationCommand, u64)>,
    refresh_outcomes: BTreeMap<uuid::Uuid, ([u8; 32], u64)>,
    dedup: BTreeMap<DedupKey, DedupRecord>,
    feature_dedup: BTreeMap<DedupKey, FeatureDedupRecord>,
}

impl ControlState {
    #[must_use]
    pub fn new(cluster_id: impl Into<String>) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            cluster_id: cluster_id.into(),
            revision: 0,
            read_schema_floor: CONTROL_SCHEMA_VERSION,
            write_schema_floor: CONTROL_SCHEMA_VERSION,
            activated_features: BTreeSet::new(),
            members: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            tabs: BTreeMap::new(),
            panes: BTreeMap::new(),
            principal_bootstrap: None,
            publication_history: Vec::new(),
            refresh_outcomes: BTreeMap::new(),
            dedup: BTreeMap::new(),
            feature_dedup: BTreeMap::new(),
        }
    }

    /// Applies a complete signed publication batch atomically in log order.
    /// # Errors
    /// Rejects stale revisions, conflicting identities, invalid authority and exhaustion.
    pub fn apply_publication(
        &mut self,
        command: &crate::capability_publication::PublicationCommand,
        membership: &openraft::StoredMembership<crate::membership::NodeId, openraft::BasicNode>,
    ) -> Result<u64, String> {
        command.encode()?;
        for (previous, revision) in &self.publication_history {
            if previous.reports == command.reports {
                return Ok(*revision);
            }
            if previous.reports.iter().any(|old| {
                command
                    .reports
                    .iter()
                    .any(|new| old.command_id == new.command_id)
            }) {
                return Err("publication command identity conflict".into());
            }
        }
        if self.publication_history.len() >= 64 {
            return Err("publication history capacity exhausted".into());
        }
        crate::capability_publication::verify_bridge(
            &command.reports,
            &self.members,
            membership,
            &self.cluster_id,
            command.verified_at_unix_ms,
        )?;
        for report in &command.reports {
            let current = self
                .publication_history
                .iter()
                .rev()
                .find_map(|(batch, _)| {
                    batch
                        .reports
                        .iter()
                        .find(|old| old.node_id == report.node_id)
                });
            let expected = match current {
                Some(old) => old
                    .expected_report_revision
                    .checked_add(1)
                    .ok_or("report revision overflow")?,
                None => 0,
            };
            if report.expected_report_revision != expected {
                return Err("publication report revision mismatch".into());
            }
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("control revision overflow")?;
        self.publication_history.push((command.clone(), revision));
        self.revision = revision;
        Ok(revision)
    }

    /// Applies a replicated bootstrap command against the committed voter configuration.
    /// No wall clock, network, or external policy service is consulted during apply.
    pub fn apply_bootstrap_command(
        &mut self,
        command: &crate::principal_bootstrap::BootstrapCommand,
        membership: &openraft::StoredMembership<crate::membership::NodeId, openraft::BasicNode>,
    ) -> ControlResponse {
        let outcome = (|| {
            // A committed successful retry remains valid after membership changes.
            if self.principal_bootstrap.is_none() {
                crate::principal_bootstrap::verify_proof(
                    &command.proof,
                    &self.cluster_id,
                    membership,
                    &self.members,
                    command.verified_at_unix_ms,
                )?;
            }
            self.apply_principal_bootstrap(
                &command.proof.statement,
                command.expected_control_revision,
            )
        })();
        let (control_revision, result) = match outcome {
            Ok(record) => (
                record.committed_revision,
                ControlCommandResult::Accepted {
                    payload: Vec::new(),
                },
            ),
            Err(reason) => (
                self.revision,
                ControlCommandResult::Rejected {
                    error: ControlCommandError::InvalidTransition { reason },
                },
            ),
        };
        ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: command.proof.statement.command_id.clone(),
            control_revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result,
        }
    }

    /// Applies a versioned refresh from the committed log; compatibility is checked
    /// before any member or retry-state mutation. Verification time is leader-assigned.
    pub fn apply_refresh_command(
        &mut self,
        command: &crate::control_codec::ProtocolRefreshCommand,
    ) -> ControlResponse {
        let outcome = if self.read_schema_floor < 4
            || self.write_schema_floor < 4
            || !self.activated_features.contains("protocol-refresh-v1")
        {
            Err("protocol refresh feature is not active".to_string())
        } else {
            self.apply_protocol_refresh(
                command.command_id,
                &command.replacement,
                &command.expected_serial,
                command.expected_revision,
                &command.node_signature,
                command.verified_at_unix_ms,
            )
        };
        let (control_revision, result) = match outcome {
            Ok(revision) => (
                revision,
                ControlCommandResult::Accepted {
                    payload: Vec::new(),
                },
            ),
            Err(reason) => (
                self.revision,
                ControlCommandResult::Rejected {
                    error: ControlCommandError::InvalidTransition { reason },
                },
            ),
        };
        ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: bmux_cluster_plugin_api::cluster_types::CommandId {
                value: command.command_id,
            },
            control_revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result,
        }
    }

    /// Applies a protocol refresh authorized by both the current node key and
    /// the existing credential issuer. The signed payload binds the predecessor
    /// credential, expected revision and complete replacement record.
    ///
    /// # Errors
    /// Rejects stale authority, invalid proof, changed identity/role or incompatible floors.
    pub fn apply_protocol_refresh(
        &mut self,
        command_id: uuid::Uuid,
        replacement: &ClusterMember,
        expected_serial: &str,
        expected_revision: u64,
        node_signature: &[u8],
        verified_at_unix_ms: u64,
    ) -> Result<u64, String> {
        let payload = Self::protocol_refresh_payload(
            command_id,
            replacement,
            expected_serial,
            expected_revision,
        )?;
        let fingerprint: [u8; 32] = sha2::Sha256::digest(&payload).into();
        if let Some((recorded, revision)) = self.refresh_outcomes.get(&command_id) {
            return if recorded == &fingerprint {
                Ok(*revision)
            } else {
                Err("refresh command identity conflict".into())
            };
        }
        if self.refresh_outcomes.len() >= 1024 {
            return Err("refresh outcome capacity exhausted".into());
        }
        let current = self
            .members
            .get(&replacement.node_id)
            .ok_or("refresh member is missing")?;
        if self.revision != expected_revision || current.credential_serial != expected_serial {
            return Err("protocol refresh precondition mismatch".into());
        }
        if current.state != ClusterMemberState::Active
            || replacement.state != ClusterMemberState::Active
            || replacement.cluster_id != self.cluster_id
            || current.cluster_id != self.cluster_id
            || current.public_key != replacement.public_key
            || current.capabilities != replacement.capabilities
            || current.endpoint != replacement.endpoint
            || current.credential_issuer_node_id != replacement.credential_issuer_node_id
            || current.credential_issuer_public_key != replacement.credential_issuer_public_key
            || replacement.credential_serial == expected_serial
            || replacement.updated_at_unix_ms <= current.updated_at_unix_ms
        {
            return Err("protocol refresh changes membership authority or is stale".into());
        }
        crate::membership::verify_membership_credential(current, verified_at_unix_ms)?;
        crate::membership::verify_membership_credential(replacement, verified_at_unix_ms)?;
        let key = current
            .public_key
            .parse::<iroh::PublicKey>()
            .map_err(|e| e.to_string())?;
        let signature = iroh::Signature::try_from(node_signature).map_err(|e| e.to_string())?;
        key.verify(&payload, &signature)
            .map_err(|_| "protocol refresh node signature is invalid")?;
        if replacement.negotiated_protocol.schema_version < u32::from(self.write_schema_floor)
            || self
                .activated_features
                .iter()
                .any(|feature| !replacement.negotiated_protocol.features.contains(feature))
        {
            return Err("protocol refresh violates active feature floors".into());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("control revision overflow")?;
        self.members
            .insert(replacement.node_id.clone(), replacement.clone());
        self.revision = revision;
        self.refresh_outcomes
            .insert(command_id, (fingerprint, revision));
        Ok(revision)
    }

    /// Canonical, domain-separated proof for the explicit protocol refresh transition.
    ///
    /// # Errors
    /// Rejects unbounded member records and predecessor identifiers.
    pub fn protocol_refresh_payload(
        command_id: uuid::Uuid,
        replacement: &ClusterMember,
        expected_serial: &str,
        expected_revision: u64,
    ) -> Result<Vec<u8>, String> {
        crate::control_codec::validate_member(replacement).map_err(|e| e.to_string())?;
        if expected_serial.len() > 1024 {
            return Err("refresh predecessor serial exceeds limit".into());
        }
        let mut writer = crate::control_codec::Writer::default();
        if command_id.is_nil() {
            return Err("refresh command identity is nil".into());
        }
        writer.raw(b"bmux.cluster.protocol-refresh.v1\0");
        writer.uuid(command_id);
        writer.string(expected_serial);
        writer.u64(expected_revision);
        writer.encode_state_member(replacement);
        Ok(writer.into_bytes())
    }

    /// Applies an already verified bootstrap statement with authoritative preconditions.
    ///
    /// This deterministic transition performs no external authorization or I/O.
    /// The consensus integration must validate proof and current membership before
    /// invoking it, and persist this state before acknowledging success.
    ///
    /// # Errors
    /// Rejects unsupported feature state, conflicts, invalid statements or revisions.
    pub fn apply_principal_bootstrap(
        &mut self,
        statement: &bmux_cluster_plugin_api::cluster_principal_bootstrap_types::BootstrapStatement,
        expected_revision: u64,
    ) -> Result<PrincipalBootstrapRecord, String> {
        let payload = crate::principal_bootstrap::signing_payload(statement)?;
        let fingerprint: [u8; 32] = sha2::Sha256::digest(&payload).into();
        if let Some(existing) = &self.principal_bootstrap {
            return if existing.command_id == statement.command_id.value
                && existing.statement_fingerprint == fingerprint
            {
                Ok(existing.clone())
            } else {
                Err("principal bootstrap has already been consumed".into())
            };
        }
        if statement.cluster_id != self.cluster_id || expected_revision != self.revision {
            return Err("bootstrap cluster or control revision mismatch".into());
        }
        if self.read_schema_floor < 3
            || self.write_schema_floor < 3
            || !self.activated_features.contains("principal-bootstrap-v1")
        {
            return Err("principal bootstrap feature is not active".into());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or("control revision overflow")?;
        let record = PrincipalBootstrapRecord {
            principal_id: statement.principal_id,
            public_key: statement.principal_public_key.clone(),
            command_id: statement.command_id.value,
            statement_fingerprint: fingerprint,
            committed_revision: revision,
        };
        self.principal_bootstrap = Some(record.clone());
        self.revision = revision;
        Ok(record)
    }

    #[must_use]
    pub fn to_view(&self, consistency: ControlReadConsistency) -> ControlStateView {
        ControlStateView {
            schema_version: self.schema_version,
            cluster_id: self.cluster_id.clone(),
            revision: self.revision,
            members: self.members.values().cloned().collect(),
            workspaces: self.workspaces.values().cloned().collect(),
            tabs: self.tabs.values().cloned().collect(),
            panes: self.panes.values().cloned().collect(),
            pending_workflows: self
                .dedup
                .iter()
                .filter(|(_, record)| {
                    record.response.workflow_status == ControlWorkflowStatus::Pending
                })
                .map(|(key, record)| PendingWorkflow {
                    principal_id: key.principal_id.clone(),
                    control_command: record.command.clone(),
                })
                .collect(),
            consistency,
        }
    }

    /// Encodes the complete deterministic state, including dedup outcomes and
    /// incomplete workflows, into its canonical snapshot representation.
    ///
    /// # Errors
    ///
    /// Returns an error if a field or collection exceeds snapshot bounds.
    pub fn encode_snapshot(&self) -> Result<Vec<u8>, StateCodecError> {
        encode_snapshot(self)
    }

    /// Restores a complete deterministic state from canonical snapshot bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, oversized, incompatible, or internally
    /// inconsistent snapshots.
    pub fn decode_snapshot(bytes: &[u8]) -> Result<Self, StateCodecError> {
        decode_snapshot(bytes)
    }

    /// Applies one already-authorized committed command deterministically.
    ///
    /// # Panics
    ///
    /// Panics if the replicated control revision overflows `u64`, which is an
    /// unrecoverable state-machine invariant violation.
    pub fn apply(&mut self, command: &ControlCommand) -> ControlResponse {
        let key = DedupKey {
            principal_id: command.principal_id.clone(),
            command_id: command.command_id.value,
        };
        let fingerprint = request_fingerprint(command);
        if let Some(existing) = self.dedup.get(&key) {
            return if existing.fingerprint == fingerprint {
                existing.response.clone()
            } else {
                self.error_response(command, ControlCommandError::CommandIdConflict)
            };
        }

        let mut next = self.clone();
        let result = if command.schema_version == CONTROL_SCHEMA_VERSION {
            next.apply_request(command)
        } else {
            Err(ControlCommandError::IncompatibleSchema {
                supported: CONTROL_SCHEMA_VERSION,
                received: command.schema_version,
            })
        };
        let (workflow_status, response_result, changed) = match result {
            Ok(outcome) => (
                outcome.workflow_status,
                ControlCommandResult::Accepted {
                    payload: outcome.payload,
                },
                outcome.changed,
            ),
            Err(error) => (
                ControlWorkflowStatus::Complete,
                ControlCommandResult::Rejected { error },
                false,
            ),
        };
        if changed {
            next.revision = self
                .revision
                .checked_add(1)
                .expect("control revision overflow is unrecoverable");
            next.set_changed_record_revisions(&command.request);
        }
        let response = ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: command.command_id.clone(),
            control_revision: next.revision,
            workflow_status,
            result: response_result,
        };
        next.dedup.insert(
            key,
            DedupRecord {
                fingerprint,
                issued_at_unix_ms: command.issued_at_unix_ms,
                command: command.clone(),
                response: response.clone(),
            },
        );
        *self = next;
        response
    }

    /// Whether this activation identity already has a committed outcome.
    /// A caller must still compare the complete command through apply; this is
    /// not permission to accept conflicting reuse.
    #[must_use]
    pub fn has_feature_activation_outcome(&self, command: &FeatureActivationCommand) -> bool {
        self.feature_dedup.contains_key(&DedupKey {
            principal_id: command.principal_id.clone(),
            command_id: command.command_id.value,
        })
    }

    /// Applies one feature-floor activation deterministically.
    ///
    /// # Panics
    ///
    /// Panics if the replicated control revision overflows `u64`, which is an
    /// unrecoverable state-machine invariant violation.
    pub fn apply_feature_activation(
        &mut self,
        command: &FeatureActivationCommand,
    ) -> ControlResponse {
        self.apply_feature_activation_with_membership(command, None)
    }

    /// Applies against the membership committed immediately before this command.
    /// Missing membership cannot authorize first bootstrap activation.
    ///
    /// # Panics
    /// Panics on unrecoverable control revision overflow.
    pub fn apply_feature_activation_with_membership(
        &mut self,
        command: &FeatureActivationCommand,
        membership: Option<
            &openraft::StoredMembership<crate::membership::NodeId, openraft::BasicNode>,
        >,
    ) -> ControlResponse {
        let key = DedupKey {
            principal_id: command.principal_id.clone(),
            command_id: command.command_id.value,
        };
        let encoded = crate::control_codec::encode_feature_activation(command);
        let fingerprint: [u8; 32] = sha2::Sha256::digest(&encoded).into();
        if let Some(existing) = self.feature_dedup.get(&key) {
            return if existing.fingerprint == fingerprint {
                existing.response.clone()
            } else {
                self.feature_error_response(command, ControlCommandError::CommandIdConflict)
            };
        }
        let result = self.validate_feature_activation(command).and_then(|()| {
            if !matches!(
                command.feature.as_str(),
                "principal-bootstrap-v1" | "protocol-refresh-v1"
            ) {
                return Ok(());
            }
            let membership = membership.ok_or_else(|| {
                invalid_transition("bootstrap activation requires committed membership")
            })?;
            let committed = membership
                .membership()
                .nodes()
                .map(|(id, _)| id.to_string())
                .collect::<BTreeSet<_>>();
            let active = self
                .members
                .values()
                .filter(|member| member.state == ClusterMemberState::Active)
                .map(|member| member.node_id.clone())
                .collect::<BTreeSet<_>>();
            if membership.log_id().is_none()
                || membership.membership().voter_ids().next().is_none()
                || committed != active
            {
                return Err(invalid_transition(
                    "bootstrap activation member records do not match committed membership",
                ));
            }
            Ok(())
        });
        let mut next = self.clone();
        let response_result = match result {
            Ok(()) => {
                next.read_schema_floor = command.read_schema_floor;
                next.write_schema_floor = command.write_schema_floor;
                next.activated_features.insert(command.feature.clone());
                next.revision = self
                    .revision
                    .checked_add(1)
                    .expect("control revision overflow is unrecoverable");
                ControlCommandResult::Accepted {
                    payload: Vec::new(),
                }
            }
            Err(error) => ControlCommandResult::Rejected { error },
        };
        let response = ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: command.command_id.clone(),
            control_revision: next.revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result: response_result,
        };
        next.feature_dedup.insert(
            key,
            FeatureDedupRecord {
                fingerprint,
                issued_at_unix_ms: command.issued_at_unix_ms,
                command: command.clone(),
                response: response.clone(),
            },
        );
        *self = next;
        response
    }

    fn validate_feature_activation(
        &self,
        command: &FeatureActivationCommand,
    ) -> Result<(), ControlCommandError> {
        require_revision(command.expected_control_revision, self.revision)?;
        if command.feature.trim().is_empty()
            || command.read_schema_floor < self.read_schema_floor
            || command.write_schema_floor < self.write_schema_floor
            || command.read_schema_floor > command.write_schema_floor
            || command.write_schema_floor <= CONTROL_SCHEMA_VERSION
        {
            return Err(invalid_transition(
                "feature activation floors or feature identity are invalid",
            ));
        }
        if matches!(
            command.feature.as_str(),
            "principal-bootstrap-v1" | "protocol-refresh-v1"
        ) {
            let minimum_schema = if command.feature == "protocol-refresh-v1" {
                4
            } else {
                3
            };
            if command.read_schema_floor < minimum_schema
                || command.write_schema_floor < minimum_schema
            {
                return Err(invalid_transition(
                    "membership feature requires its supported schema floor",
                ));
            }
            let active = self
                .members
                .values()
                .filter(|member| member.state == ClusterMemberState::Active)
                .collect::<Vec<_>>();
            if active.is_empty()
                || active.iter().any(|member| {
                    member.cluster_id != self.cluster_id
                        || member.negotiated_protocol.schema_version
                            < u32::from(command.write_schema_floor)
                        || !member
                            .negotiated_protocol
                            .features
                            .contains(&command.feature)
                })
            {
                return Err(invalid_transition(
                    "principal bootstrap requires compatible active members",
                ));
            }
        }
        Ok(())
    }

    fn feature_error_response(
        &self,
        command: &FeatureActivationCommand,
        error: ControlCommandError,
    ) -> ControlResponse {
        ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: command.command_id.clone(),
            control_revision: self.revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result: ControlCommandResult::Rejected { error },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_request(
        &mut self,
        command: &ControlCommand,
    ) -> Result<ApplyOutcome, ControlCommandError> {
        match &command.request {
            ControlCommandRequest::UpsertMember { member } => {
                if member.cluster_id != self.cluster_id {
                    return Err(invalid_transition("member belongs to a different cluster"));
                }
                crate::membership::verify_membership_credential(member, command.issued_at_unix_ms)
                    .map_err(|error| {
                        invalid_transition(&format!("member credential is invalid: {error}"))
                    })?;
                if let Some(endpoint) = member.endpoint.as_deref() {
                    crate::membership::validate_advertised_endpoint(endpoint).map_err(|error| {
                        invalid_transition(&format!("member endpoint is invalid: {error}"))
                    })?;
                    if self.members.values().any(|existing| {
                        existing.node_id != member.node_id
                            && existing.state == ClusterMemberState::Active
                            && existing.endpoint.as_deref() == Some(endpoint)
                    }) {
                        return Err(invalid_transition(
                            "member endpoint is already assigned to another active member",
                        ));
                    }
                } else if member.state == ClusterMemberState::Active
                    && member.capabilities.consensus_role
                        == bmux_cluster_plugin_api::cluster_types::ClusterConsensusRole::Voter
                {
                    return Err(invalid_transition(
                        "active voter requires an advertised endpoint",
                    ));
                }
                if let Some(existing) = self.members.get(&member.node_id) {
                    if existing.negotiated_protocol != member.negotiated_protocol {
                        return Err(invalid_transition(
                            "protocol capability changes require an explicit authenticated refresh transition",
                        ));
                    }
                    if existing.state == ClusterMemberState::Active
                        && existing.capabilities.consensus_role
                            == bmux_cluster_plugin_api::cluster_types::ClusterConsensusRole::Voter
                        && existing.endpoint != member.endpoint
                    {
                        return Err(invalid_transition(
                            "active voter endpoint cannot be rewritten by membership publication",
                        ));
                    }
                    if matches!(
                        existing.state,
                        ClusterMemberState::Revoked | ClusterMemberState::Left
                    ) && member.state == ClusterMemberState::Active
                    {
                        return Err(invalid_transition(
                            "inactive member cannot be reactivated by upsert",
                        ));
                    }
                    if member.updated_at_unix_ms < existing.updated_at_unix_ms {
                        return Err(invalid_transition(
                            "membership update is older than replicated state",
                        ));
                    }
                    if member.updated_at_unix_ms == existing.updated_at_unix_ms
                        && member != existing
                    {
                        return Err(invalid_transition(
                            "membership update conflicts at the same timestamp",
                        ));
                    }
                }
                if member.state == ClusterMemberState::Active
                    && (member.negotiated_protocol.schema_version
                        < u32::from(self.write_schema_floor)
                        || self.activated_features.iter().any(|feature| {
                            !member
                                .negotiated_protocol
                                .features
                                .iter()
                                .any(|supported| supported == feature)
                        }))
                {
                    return Err(invalid_transition(
                        "active member does not satisfy the cluster write floor",
                    ));
                }
                let changed = self.members.get(&member.node_id) != Some(member);
                self.members.insert(member.node_id.clone(), member.clone());
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::SetMemberState {
                node_id,
                expected_credential_serial,
                state,
            } => {
                let member = self
                    .members
                    .get_mut(node_id)
                    .ok_or_else(|| not_found(ControlResourceKind::Member, node_id.clone()))?;
                if member.credential_serial != *expected_credential_serial {
                    return Err(invalid_transition("member credential serial is stale"));
                }
                if !valid_member_transition(member.state, *state) {
                    return Err(invalid_transition("member state transition is invalid"));
                }
                let changed = member.state != *state;
                member.state = *state;
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::CreateWorkspace { workspace_id, name } => {
                if self.workspaces.contains_key(&workspace_id.value) {
                    return Err(already_exists(
                        ControlResourceKind::Workspace,
                        workspace_id.value.to_string(),
                    ));
                }
                self.workspaces.insert(
                    workspace_id.value,
                    WorkspaceRecord {
                        workspace_id: workspace_id.clone(),
                        name: name.clone(),
                        revision: 0,
                    },
                );
                Ok(ApplyOutcome::complete(true))
            }
            ControlCommandRequest::RenameWorkspace {
                workspace_id,
                expected_revision,
                name,
            } => {
                let workspace = workspace_mut(self, workspace_id)?;
                require_revision(*expected_revision, workspace.revision)?;
                let changed = workspace.name != *name;
                workspace.name.clone_from(name);
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::PutTab {
                tab,
                expected_workspace_revision,
            } => {
                let workspace = workspace_mut(self, &tab.workspace_id)?;
                require_revision(*expected_workspace_revision, workspace.revision)?;
                let changed = self.tabs.get(&tab.tab_id.value) != Some(tab);
                self.tabs.insert(tab.tab_id.value, tab.clone());
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::RemoveTab {
                tab_id,
                expected_workspace_revision,
            } => {
                let tab = self
                    .tabs
                    .get(&tab_id.value)
                    .ok_or_else(|| not_found(ControlResourceKind::Tab, tab_id.value.to_string()))?;
                let workspace_id = tab.workspace_id.clone();
                require_revision(
                    *expected_workspace_revision,
                    workspace(self, &workspace_id)?.revision,
                )?;
                if self
                    .panes
                    .values()
                    .any(|pane| pane.tab_id.value == tab_id.value)
                {
                    return Err(invalid_transition("tab still contains logical panes"));
                }
                self.tabs.remove(&tab_id.value);
                Ok(ApplyOutcome::complete(true))
            }
            ControlCommandRequest::PutPane {
                pane,
                expected_workspace_revision,
            } => {
                require_pane_references(self, pane)?;
                require_revision(
                    *expected_workspace_revision,
                    workspace(self, &pane.workspace_id)?.revision,
                )?;
                validate_execution(pane)?;
                let changed = self.panes.get(&pane.pane_id.value) != Some(pane);
                self.panes.insert(pane.pane_id.value, pane.clone());
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::RemovePane {
                pane_id,
                expected_revision,
                expected_generation,
            } => {
                let pane = self.panes.get(&pane_id.value).ok_or_else(|| {
                    not_found(ControlResourceKind::Pane, pane_id.value.to_string())
                })?;
                require_revision(*expected_revision, pane.revision)?;
                if let Some(expected) = expected_generation {
                    require_generation(*expected, pane_generation(pane))?;
                }
                self.panes.remove(&pane_id.value);
                Ok(ApplyOutcome::complete(true))
            }
            ControlCommandRequest::AssignExecution {
                pane_id,
                expected_revision,
                expected_generation,
                assignment,
                launch_spec,
            } => {
                if launch_spec.is_none() {
                    return Err(invalid_transition(
                        "execution assignment requires a durable launch specification",
                    ));
                }
                let pane = self.panes.get_mut(&pane_id.value).ok_or_else(|| {
                    not_found(ControlResourceKind::Pane, pane_id.value.to_string())
                })?;
                require_revision(*expected_revision, pane.revision)?;
                let current_generation = pane_generation(pane);
                require_generation(*expected_generation, current_generation)?;
                if assignment.generation <= current_generation {
                    return Err(ControlCommandError::GenerationConflict {
                        expected: current_generation.saturating_add(1),
                        current: assignment.generation,
                    });
                }
                pane.execution = Some(assignment.clone());
                pane.availability = PaneAvailability::Pending;
                pane.availability_reason = None;
                Ok(ApplyOutcome::pending(true))
            }
            ControlCommandRequest::SetPaneAvailability {
                pane_id,
                expected_revision,
                assignment,
                availability,
                reason,
            } => {
                let pane = self.panes.get_mut(&pane_id.value).ok_or_else(|| {
                    not_found(ControlResourceKind::Pane, pane_id.value.to_string())
                })?;
                require_revision(*expected_revision, pane.revision)?;
                if pane.execution.as_ref() != Some(assignment) {
                    return Err(ControlCommandError::GenerationConflict {
                        expected: pane_generation(pane),
                        current: assignment.generation,
                    });
                }
                let changed =
                    pane.availability != *availability || pane.availability_reason != *reason;
                pane.availability = *availability;
                pane.availability_reason.clone_from(reason);
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::CompleteWorkflow {
                original_command_id,
                response,
            } => {
                let original_key = DedupKey {
                    principal_id: command.principal_id.clone(),
                    command_id: original_command_id.value,
                };
                let original = self.dedup.get_mut(&original_key).ok_or_else(|| {
                    not_found(
                        ControlResourceKind::Workflow,
                        original_command_id.value.to_string(),
                    )
                })?;
                if original.response.workflow_status == ControlWorkflowStatus::Complete {
                    return Ok(ApplyOutcome::complete(false));
                }
                original.response.workflow_status = ControlWorkflowStatus::Complete;
                original.response.result = ControlCommandResult::Accepted {
                    payload: response.clone(),
                };
                Ok(ApplyOutcome::complete(true))
            }
            ControlCommandRequest::PruneDedup {
                completed_before_unix_ms,
            } => {
                let before = self.dedup.len().saturating_add(self.feature_dedup.len());
                self.dedup.retain(|_, record| {
                    record.response.workflow_status == ControlWorkflowStatus::Pending
                        || record.issued_at_unix_ms >= *completed_before_unix_ms
                });
                self.feature_dedup
                    .retain(|_, record| record.issued_at_unix_ms >= *completed_before_unix_ms);
                Ok(ApplyOutcome::complete(
                    self.dedup.len().saturating_add(self.feature_dedup.len()) != before,
                ))
            }
        }
    }

    fn set_changed_record_revisions(&mut self, request: &ControlCommandRequest) {
        let revision = self.revision;
        match request {
            ControlCommandRequest::CreateWorkspace { workspace_id, .. }
            | ControlCommandRequest::RenameWorkspace { workspace_id, .. } => {
                if let Some(workspace) = self.workspaces.get_mut(&workspace_id.value) {
                    workspace.revision = revision;
                }
            }
            ControlCommandRequest::PutTab { tab, .. } => {
                if let Some(stored) = self.tabs.get_mut(&tab.tab_id.value) {
                    stored.revision = revision;
                }
                if let Some(workspace) = self.workspaces.get_mut(&tab.workspace_id.value) {
                    workspace.revision = revision;
                }
            }
            ControlCommandRequest::PutPane { pane, .. } => {
                if let Some(stored) = self.panes.get_mut(&pane.pane_id.value) {
                    stored.revision = revision;
                }
                if let Some(workspace) = self.workspaces.get_mut(&pane.workspace_id.value) {
                    workspace.revision = revision;
                }
            }
            ControlCommandRequest::AssignExecution { pane_id, .. }
            | ControlCommandRequest::SetPaneAvailability { pane_id, .. } => {
                if let Some(pane) = self.panes.get_mut(&pane_id.value) {
                    pane.revision = revision;
                }
            }
            ControlCommandRequest::UpsertMember { .. }
            | ControlCommandRequest::SetMemberState { .. }
            | ControlCommandRequest::RemoveTab { .. }
            | ControlCommandRequest::RemovePane { .. }
            | ControlCommandRequest::CompleteWorkflow { .. }
            | ControlCommandRequest::PruneDedup { .. } => {}
        }
    }

    fn error_response(
        &self,
        command: &ControlCommand,
        error: ControlCommandError,
    ) -> ControlResponse {
        ControlResponse {
            schema_version: CONTROL_SCHEMA_VERSION,
            command_id: command.command_id.clone(),
            control_revision: self.revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result: ControlCommandResult::Rejected { error },
        }
    }
}

#[derive(Debug)]
struct ApplyOutcome {
    changed: bool,
    workflow_status: ControlWorkflowStatus,
    payload: Vec<u8>,
}

impl ApplyOutcome {
    const fn complete(changed: bool) -> Self {
        Self {
            changed,
            workflow_status: ControlWorkflowStatus::Complete,
            payload: Vec::new(),
        }
    }

    const fn pending(changed: bool) -> Self {
        Self {
            changed,
            workflow_status: ControlWorkflowStatus::Pending,
            payload: Vec::new(),
        }
    }
}

fn workspace<'a>(
    state: &'a ControlState,
    workspace_id: &WorkspaceId,
) -> Result<&'a WorkspaceRecord, ControlCommandError> {
    state.workspaces.get(&workspace_id.value).ok_or_else(|| {
        not_found(
            ControlResourceKind::Workspace,
            workspace_id.value.to_string(),
        )
    })
}

fn workspace_mut<'a>(
    state: &'a mut ControlState,
    workspace_id: &WorkspaceId,
) -> Result<&'a mut WorkspaceRecord, ControlCommandError> {
    state
        .workspaces
        .get_mut(&workspace_id.value)
        .ok_or_else(|| {
            not_found(
                ControlResourceKind::Workspace,
                workspace_id.value.to_string(),
            )
        })
}

fn require_pane_references(
    state: &ControlState,
    pane: &LogicalPaneRecord,
) -> Result<(), ControlCommandError> {
    workspace(state, &pane.workspace_id)?;
    let tab = state.tabs.get(&pane.tab_id.value).ok_or_else(|| {
        ControlCommandError::InvalidReference {
            resource: ControlResourceKind::Tab,
            id: pane.tab_id.value.to_string(),
        }
    })?;
    if tab.workspace_id != pane.workspace_id {
        return Err(ControlCommandError::InvalidReference {
            resource: ControlResourceKind::Workspace,
            id: pane.workspace_id.value.to_string(),
        });
    }
    Ok(())
}

fn validate_execution(pane: &LogicalPaneRecord) -> Result<(), ControlCommandError> {
    if pane
        .execution
        .as_ref()
        .is_some_and(|assignment| assignment.generation == 0)
    {
        return Err(ControlCommandError::GenerationConflict {
            expected: 1,
            current: 0,
        });
    }
    Ok(())
}

fn pane_generation(pane: &LogicalPaneRecord) -> u64 {
    pane.execution
        .as_ref()
        .map_or(0, |assignment| assignment.generation)
}

const fn require_revision(expected: u64, current: u64) -> Result<(), ControlCommandError> {
    if expected == current {
        Ok(())
    } else {
        Err(ControlCommandError::RevisionConflict { expected, current })
    }
}

const fn require_generation(expected: u64, current: u64) -> Result<(), ControlCommandError> {
    if expected == current {
        Ok(())
    } else {
        Err(ControlCommandError::GenerationConflict { expected, current })
    }
}

const fn valid_member_transition(from: ClusterMemberState, to: ClusterMemberState) -> bool {
    matches!(
        (from, to),
        (
            ClusterMemberState::Active,
            ClusterMemberState::Active | ClusterMemberState::Revoked | ClusterMemberState::Left
        ) | (
            ClusterMemberState::Revoked,
            ClusterMemberState::Revoked | ClusterMemberState::Left
        ) | (ClusterMemberState::Left, ClusterMemberState::Left)
    )
}

const fn not_found(resource: ControlResourceKind, id: String) -> ControlCommandError {
    ControlCommandError::NotFound { resource, id }
}

const fn already_exists(resource: ControlResourceKind, id: String) -> ControlCommandError {
    ControlCommandError::AlreadyExists { resource, id }
}

fn invalid_transition(reason: &str) -> ControlCommandError {
    ControlCommandError::InvalidTransition {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::{
        CommandId, ExecutionAssignment, ExecutionId, LogicalPaneId, LogicalTabId, PaneAvailability,
        PaneRestartPolicy, PlacementIntent, WorkerLaunchSpec,
    };

    fn id(value: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(value)
    }

    fn command(id_value: u64, request: ControlCommandRequest) -> ControlCommand {
        ControlCommand {
            schema_version: 1,
            principal_id: "principal:test".to_string(),
            command_id: CommandId {
                value: id(u128::from(id_value)),
            },
            issued_at_unix_ms: id_value,
            request,
        }
    }

    fn create_workspace(id_value: u128) -> ControlCommandRequest {
        ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId { value: id(10) },
            name: Some(format!("workspace-{id_value}")),
        }
    }

    fn launch_spec() -> WorkerLaunchSpec {
        WorkerLaunchSpec {
            program: Some("sh".to_string()),
            args: vec!["-lc".to_string(), "printf ready".to_string()],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }

    fn setup_pane(state: &mut ControlState) {
        assert_accepted(&state.apply(&command(1, create_workspace(1))));
        assert_accepted(&state.apply(&command(
            2,
            ControlCommandRequest::PutTab {
                tab: LogicalTabRecord {
                    tab_id: LogicalTabId { value: id(20) },
                    workspace_id: WorkspaceId { value: id(10) },
                    name: None,
                    layout_schema_version: 1,
                    layout: Vec::new(),
                    revision: 0,
                },
                expected_workspace_revision: 1,
            },
        )));
        assert_accepted(&state.apply(&command(
            3,
            ControlCommandRequest::PutPane {
                pane: LogicalPaneRecord {
                    pane_id: LogicalPaneId { value: id(30) },
                    workspace_id: WorkspaceId { value: id(10) },
                    tab_id: LogicalTabId { value: id(20) },
                    name: None,
                    restart_policy: PaneRestartPolicy::Manual,
                    placement: PlacementIntent {
                        explicit_node_id: None,
                        required_labels: Vec::new(),
                        preferred_labels: Vec::new(),
                    },
                    availability: PaneAvailability::Pending,
                    availability_reason: None,
                    execution: None,
                    revision: 0,
                },
                expected_workspace_revision: 2,
            },
        )));
    }

    #[test]
    fn independent_state_machines_apply_identical_sequences() {
        let commands = [
            command(1, create_workspace(1)),
            command(
                2,
                ControlCommandRequest::RenameWorkspace {
                    workspace_id: WorkspaceId { value: id(10) },
                    expected_revision: 1,
                    name: Some("renamed".to_string()),
                },
            ),
        ];
        let mut first = ControlState::new("cluster:test");
        let mut second = ControlState::new("cluster:test");
        let first_responses: Vec<_> = commands.iter().map(|entry| first.apply(entry)).collect();
        let second_responses: Vec<_> = commands.iter().map(|entry| second.apply(entry)).collect();
        assert_eq!(first, second);
        assert_eq!(first_responses, second_responses);
    }

    #[test]
    fn invalid_references_and_future_schema_fail_without_mutation() {
        let mut state = ControlState::new("cluster:test");
        let invalid = command(
            1,
            ControlCommandRequest::PutPane {
                pane: LogicalPaneRecord {
                    pane_id: LogicalPaneId { value: id(30) },
                    workspace_id: WorkspaceId { value: id(10) },
                    tab_id: LogicalTabId { value: id(20) },
                    name: None,
                    restart_policy: PaneRestartPolicy::Manual,
                    placement: PlacementIntent {
                        explicit_node_id: None,
                        required_labels: Vec::new(),
                        preferred_labels: Vec::new(),
                    },
                    availability: PaneAvailability::Pending,
                    availability_reason: None,
                    execution: None,
                    revision: 0,
                },
                expected_workspace_revision: 0,
            },
        );
        assert!(matches!(
            state.apply(&invalid).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::NotFound { .. }
            }
        ));
        assert!(state.panes.is_empty());
        assert_eq!(state.revision, 0);

        let mut future = command(2, create_workspace(2));
        future.schema_version = 2;
        assert!(matches!(
            state.apply(&future).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::IncompatibleSchema { .. }
            }
        ));
        assert!(state.workspaces.is_empty());
        assert_eq!(state.revision, 0);
    }

    #[test]
    fn duplicate_delivery_returns_one_outcome_and_conflicts_fail() {
        let mut state = ControlState::new("cluster:test");
        let original = command(1, create_workspace(1));
        let response = state.apply(&original);
        assert_eq!(state.revision, 1);
        assert_eq!(state.apply(&original), response);
        assert_eq!(state.revision, 1);

        let conflict = command(1, create_workspace(2));
        assert!(matches!(
            state.apply(&conflict).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::CommandIdConflict
            }
        ));
        assert_eq!(state.revision, 1);
    }

    #[test]
    fn unavailable_state_preserves_layout_and_authoritative_execution() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let assigned = state.apply(&command(
            30,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 3,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        ));
        assert_eq!(assigned.workflow_status, ControlWorkflowStatus::Pending);
        let current = state.panes.get(&id(30)).unwrap().clone();
        let unavailable = state.apply(&command(
            31,
            ControlCommandRequest::SetPaneAvailability {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: current.revision,
                assignment: current.execution.clone().unwrap(),
                availability: PaneAvailability::Unavailable,
                reason: Some("worker unreachable; process state unknown".to_string()),
            },
        ));
        assert_accepted(&unavailable);
        let pane = state.panes.get(&id(30)).unwrap();
        assert_eq!(pane.availability, PaneAvailability::Unavailable);
        assert_eq!(pane.execution, current.execution);
        assert_eq!(pane.workspace_id.value, id(10));
        assert_eq!(pane.tab_id.value, id(20));
        assert!(state.tabs.contains_key(&id(20)));
        assert!(state.workspaces.contains_key(&id(10)));

        let rejected = state.apply(&command(
            32,
            ControlCommandRequest::SetPaneAvailability {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: pane.revision,
                assignment: assignment(2),
                availability: PaneAvailability::Ready,
                reason: None,
            },
        ));
        assert!(matches!(
            rejected.result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::GenerationConflict { .. }
            }
        ));
        assert_eq!(
            state.panes.get(&id(30)).unwrap().availability,
            PaneAvailability::Unavailable
        );
    }

    #[test]
    fn revision_and_generation_fencing_are_strict() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let stale_revision = state.apply(&command(
            4,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 2,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        ));
        assert!(matches!(
            stale_revision.result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::RevisionConflict { .. }
            }
        ));
        let assigned = state.apply(&command(
            5,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 3,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        ));
        assert_eq!(assigned.workflow_status, ControlWorkflowStatus::Pending);
        let reused = state.apply(&command(
            6,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 4,
                expected_generation: 1,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        ));
        assert!(matches!(
            reused.result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::GenerationConflict { .. }
            }
        ));
    }

    #[test]
    fn stale_execution_cannot_change_availability() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        assert_eq!(
            state
                .apply(&command(
                    4,
                    ControlCommandRequest::AssignExecution {
                        pane_id: LogicalPaneId { value: id(30) },
                        expected_revision: 3,
                        expected_generation: 0,
                        assignment: assignment(1),
                        launch_spec: Some(launch_spec()),
                    },
                ))
                .workflow_status,
            ControlWorkflowStatus::Pending
        );
        let stale_execution = state.apply(&command(
            5,
            ControlCommandRequest::SetPaneAvailability {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 4,
                assignment: assignment(2),
                availability: PaneAvailability::Ready,
                reason: None,
            },
        ));
        assert!(matches!(
            stale_execution.result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::GenerationConflict { .. }
            }
        ));
        assert_eq!(state.panes[&id(30)].availability, PaneAvailability::Pending);
    }

    fn assert_protocol_refresh_rejected(
        state: &mut ControlState,
        identity: &crate::membership::NodeIdentity,
        cluster_id: crate::membership::ClusterId,
        member: &bmux_cluster_plugin_api::cluster_types::ClusterMember,
        issued_at_unix_ms: u64,
    ) {
        let mut protocol = member.negotiated_protocol.clone();
        protocol.schema_version += 1;
        let mut refreshed = crate::membership::issue_membership_credential(
            identity,
            cluster_id,
            identity.node_id().to_string(),
            identity.public_key().to_string(),
            crate::membership::initializer_capabilities(),
            protocol,
            issued_at_unix_ms + 1,
        )
        .unwrap();
        refreshed.endpoint.clone_from(&member.endpoint);
        let command_id = uuid::Uuid::new_v4();
        let payload = ControlState::protocol_refresh_payload(
            command_id,
            &refreshed,
            &member.credential_serial,
            state.revision,
        )
        .unwrap();
        let mut refreshed_state = state.clone();
        assert!(
            refreshed_state
                .apply_protocol_refresh(
                    command_id,
                    &refreshed,
                    &member.credential_serial,
                    state.revision,
                    &[0; 64],
                    issued_at_unix_ms + 1
                )
                .is_err()
        );
        assert_eq!(refreshed_state, *state);
        refreshed_state
            .apply_protocol_refresh(
                command_id,
                &refreshed,
                &member.credential_serial,
                state.revision,
                &identity.sign(&payload),
                issued_at_unix_ms + 1,
            )
            .unwrap();
        assert_eq!(refreshed_state.members[&member.node_id], refreshed);
        assert_eq!(refreshed_state.revision, state.revision + 1);
        let bytes = refreshed_state.encode_snapshot().unwrap();
        let mut restored = ControlState::decode_snapshot(&bytes).unwrap();
        assert_eq!(restored, refreshed_state);
        assert_eq!(
            restored
                .apply_protocol_refresh(
                    command_id,
                    &refreshed,
                    &member.credential_serial,
                    state.revision,
                    &identity.sign(&payload),
                    issued_at_unix_ms + 1
                )
                .unwrap(),
            refreshed_state.revision
        );
        let mut conflicting = refreshed.clone();
        conflicting.updated_at_unix_ms += 1;
        assert!(
            restored
                .apply_protocol_refresh(
                    command_id,
                    &conflicting,
                    &member.credential_serial,
                    state.revision,
                    &identity.sign(&payload),
                    issued_at_unix_ms + 1
                )
                .is_err()
        );
        let mut refresh = command(
            55,
            ControlCommandRequest::UpsertMember { member: refreshed },
        );
        refresh.issued_at_unix_ms = issued_at_unix_ms + 1;
        assert!(
            matches!(state.apply(&refresh).result, ControlCommandResult::Rejected {
            error: ControlCommandError::InvalidTransition { ref reason }
        } if reason.contains("explicit authenticated refresh"))
        );
    }

    #[test]
    fn membership_updates_reject_stale_and_same_timestamp_conflicts() {
        let identity = crate::membership::NodeIdentity::new_for_test(1);
        let cluster_id = "cluster:00000000-0000-0000-0000-000000000001"
            .parse::<crate::membership::ClusterId>()
            .unwrap();
        let mut state = ControlState::new(cluster_id.to_string());
        let mut member = crate::membership::issue_membership_credential(
            &identity,
            cluster_id,
            identity.node_id().to_string(),
            identity.public_key().to_string(),
            crate::membership::initializer_capabilities(),
            bmux_cluster_plugin_api::cluster_types::ClusterNegotiatedProtocol {
                wire_epoch: 1,
                peer_revision: 1,
                schema_version: 1,
                local_plugin_version: "test".to_string(),
                remote_plugin_version: "test".to_string(),
                features: Vec::new(),
            },
            crate::now_unix_ms(),
        )
        .unwrap();
        member.endpoint = Some("tls://member.example:7443".to_string());
        let issued_at_unix_ms = member.updated_at_unix_ms;
        let mut initial = command(
            50,
            ControlCommandRequest::UpsertMember {
                member: member.clone(),
            },
        );
        initial.issued_at_unix_ms = issued_at_unix_ms;
        assert_accepted(&state.apply(&initial));

        let mut older = member.clone();
        older.updated_at_unix_ms -= 1;
        let mut older_command = command(51, ControlCommandRequest::UpsertMember { member: older });
        older_command.issued_at_unix_ms = issued_at_unix_ms;
        assert!(matches!(
            state.apply(&older_command).result,
            ControlCommandResult::Rejected { .. }
        ));
        let mut conflicting = member.clone();
        conflicting.endpoint = Some("tls://different.example:7443".to_string());
        conflicting.updated_at_unix_ms += 1;
        let mut conflicting_command = command(
            52,
            ControlCommandRequest::UpsertMember {
                member: conflicting,
            },
        );
        conflicting_command.issued_at_unix_ms = issued_at_unix_ms;
        assert!(matches!(
            state.apply(&conflicting_command).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::InvalidTransition { ref reason }
            } if reason.contains("endpoint cannot be rewritten")
        ));

        let mut wrong_node_id = member.clone();
        wrong_node_id.node_id = crate::membership::NodeIdentity::new_for_test(2)
            .node_id()
            .to_string();
        wrong_node_id.updated_at_unix_ms += 1;
        let mut wrong_node_command = command(
            53,
            ControlCommandRequest::UpsertMember {
                member: wrong_node_id,
            },
        );
        wrong_node_command.issued_at_unix_ms = issued_at_unix_ms;
        assert!(matches!(
            state.apply(&wrong_node_command).result,
            ControlCommandResult::Rejected { .. }
        ));

        assert_protocol_refresh_rejected(
            &mut state,
            &identity,
            cluster_id,
            &member,
            issued_at_unix_ms,
        );

        let duplicate_identity = crate::membership::NodeIdentity::new_for_test(2);
        let mut duplicate = crate::membership::issue_membership_credential(
            &identity,
            cluster_id,
            duplicate_identity.node_id().to_string(),
            duplicate_identity.public_key().to_string(),
            crate::membership::initializer_capabilities(),
            member.negotiated_protocol.clone(),
            issued_at_unix_ms,
        )
        .unwrap();
        duplicate.endpoint.clone_from(&member.endpoint);
        let mut duplicate_command = command(
            54,
            ControlCommandRequest::UpsertMember { member: duplicate },
        );
        duplicate_command.issued_at_unix_ms = issued_at_unix_ms;
        assert!(matches!(
            state.apply(&duplicate_command).result,
            ControlCommandResult::Rejected { .. }
        ));
    }

    #[test]
    fn snapshot_round_trip_is_canonical_and_preserves_pending_dedup() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let pending = command(
            20,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 3,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        );
        assert_eq!(
            state.apply(&pending).workflow_status,
            ControlWorkflowStatus::Pending
        );

        let first = state.encode_snapshot().unwrap();
        let pending_view = state.to_view(ControlReadConsistency::Linearizable);
        assert_eq!(pending_view.pending_workflows.len(), 1);
        assert_eq!(
            pending_view.pending_workflows[0].principal_id,
            "principal:test"
        );
        assert_eq!(pending_view.pending_workflows[0].control_command, pending);
        let restored = ControlState::decode_snapshot(&first).unwrap();
        assert_eq!(
            restored
                .to_view(ControlReadConsistency::Linearizable)
                .pending_workflows,
            pending_view.pending_workflows
        );
        assert_eq!(restored, state);
        assert_eq!(restored.encode_snapshot().unwrap(), first);

        let mut restored = restored;
        assert_eq!(
            restored.apply(&pending).workflow_status,
            ControlWorkflowStatus::Pending
        );
        let completion = command(
            21,
            ControlCommandRequest::CompleteWorkflow {
                original_command_id: pending.command_id.clone(),
                response: vec![4, 5, 6],
            },
        );
        assert_accepted(&restored.apply(&completion));
        let restored_again =
            ControlState::decode_snapshot(&restored.encode_snapshot().unwrap()).unwrap();
        assert_eq!(restored_again, restored);
    }

    #[test]
    fn legacy_snapshot_migrates_idempotently_to_current_format() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let current = state.encode_snapshot().unwrap();
        let mut legacy = Vec::with_capacity(current.len() - 4);
        legacy.extend_from_slice(LEGACY_SNAPSHOT_MAGIC);
        legacy.extend_from_slice(&current[12..]);

        let migrated = ControlState::decode_snapshot(&legacy).unwrap();
        assert_eq!(migrated, state);
        let canonical = migrated.encode_snapshot().unwrap();
        assert_eq!(&canonical[..8], PREVIOUS_SNAPSHOT_MAGIC);
        assert_eq!(ControlState::decode_snapshot(&canonical).unwrap(), migrated);
        assert_eq!(
            ControlState::decode_snapshot(&canonical)
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            canonical
        );
    }

    #[test]
    fn bootstrap_activation_rejects_missing_learner_record() {
        use crate::membership::{
            ClusterId, NodeIdentity, initializer_capabilities, issue_test_member,
        };
        use openraft::{BasicNode, CommittedLeaderId, LogId, Membership, StoredMembership};
        let identity = NodeIdentity::new_for_test(96);
        let id = *identity.node_id();
        let learner = crate::membership::NodeId::from(97);
        let cluster: ClusterId = "cluster:00000000-0000-0000-0000-000000000096"
            .parse()
            .unwrap();
        let mut state = ControlState::new(cluster.to_string());
        let member = issue_test_member(
            &identity,
            cluster,
            &identity,
            "tls://127.0.0.1:49996",
            initializer_capabilities(),
            42,
        );
        state.members.insert(member.node_id.clone(), member);
        let membership = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(1, id), 1)),
            Membership::new(
                vec![BTreeSet::from([id])],
                BTreeMap::from([
                    (id, BasicNode::new("node")),
                    (learner, BasicNode::new("learner")),
                ]),
            ),
        );
        let command = FeatureActivationCommand {
            principal_id: id.to_string(),
            command_id: CommandId {
                value: uuid::Uuid::new_v4(),
            },
            issued_at_unix_ms: 42,
            expected_control_revision: 0,
            read_schema_floor: 3,
            write_schema_floor: 3,
            feature: "principal-bootstrap-v1".into(),
        };
        let response = state.apply_feature_activation_with_membership(&command, Some(&membership));
        assert!(matches!(
            response.result,
            ControlCommandResult::Rejected { .. }
        ));
        assert_eq!(state.revision, 0);
        assert!(!state.activated_features.contains("principal-bootstrap-v1"));
        assert_eq!(
            state.apply_feature_activation_with_membership(&command, None),
            response
        );
    }

    #[test]
    fn bootstrap_activation_requires_replicated_compatible_members() {
        let mut state = ControlState::new("cluster:test");
        let command = FeatureActivationCommand {
            principal_id: "principal:test".into(),
            command_id: CommandId { value: id(98) },
            issued_at_unix_ms: 42,
            expected_control_revision: 0,
            read_schema_floor: 3,
            write_schema_floor: 3,
            feature: "principal-bootstrap-v1".into(),
        };
        assert!(matches!(
            state.apply_feature_activation(&command).result,
            ControlCommandResult::Rejected { .. }
        ));
        assert_eq!(state.write_schema_floor, 1);
        assert!(state.activated_features.is_empty());
        assert_eq!(state.revision, 0);
    }

    #[test]
    fn refresh_activation_requires_schema_and_exact_committed_membership() {
        use crate::membership::{
            ClusterId, NodeIdentity, initializer_capabilities, issue_test_member,
        };
        use openraft::{BasicNode, CommittedLeaderId, LogId, Membership, StoredMembership};
        let identity = NodeIdentity::new_for_test(96);
        let node = *identity.node_id();
        let cluster: ClusterId = "cluster:00000000-0000-0000-0000-000000000096"
            .parse()
            .unwrap();
        let mut state = ControlState::new(cluster.to_string());
        let mut member = issue_test_member(
            &identity,
            cluster,
            &identity,
            "tls://127.0.0.1:49996",
            initializer_capabilities(),
            42,
        );
        // Activation consumes already committed capability records; credential
        // validation belongs to the member installation transition.
        member.negotiated_protocol.schema_version = 4;
        member
            .negotiated_protocol
            .features
            .push("protocol-refresh-v1".into());
        state.members.insert(member.node_id.clone(), member);
        let membership = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(1, node), 1)),
            Membership::new(
                vec![BTreeSet::from([node])],
                BTreeMap::from([(node, BasicNode::new("node"))]),
            ),
        );
        let command = FeatureActivationCommand {
            principal_id: node.to_string(),
            command_id: CommandId { value: id(96) },
            issued_at_unix_ms: 42,
            expected_control_revision: 0,
            read_schema_floor: 4,
            write_schema_floor: 4,
            feature: "protocol-refresh-v1".into(),
        };
        let rejected =
            |state: &mut ControlState, command: &FeatureActivationCommand, membership| {
                assert!(matches!(
                    state
                        .apply_feature_activation_with_membership(command, membership)
                        .result,
                    ControlCommandResult::Rejected { .. }
                ));
                assert_eq!(state.revision, 0);
                assert_eq!(state.write_schema_floor, 1);
                assert!(!state.activated_features.contains("protocol-refresh-v1"));
            };
        rejected(&mut state.clone(), &command, None);
        let mut old_floor = command.clone();
        old_floor.read_schema_floor = 3;
        old_floor.write_schema_floor = 3;
        rejected(&mut state.clone(), &old_floor, Some(&membership));
        let mut incompatible = state.clone();
        incompatible
            .members
            .get_mut(&node.to_string())
            .unwrap()
            .negotiated_protocol
            .schema_version = 3;
        rejected(&mut incompatible, &command, Some(&membership));
        let mut missing_feature = state.clone();
        missing_feature
            .members
            .get_mut(&node.to_string())
            .unwrap()
            .negotiated_protocol
            .features
            .clear();
        rejected(&mut missing_feature, &command, Some(&membership));
        let learner = crate::membership::NodeId::from(97);
        let mismatch = StoredMembership::new(
            Some(LogId::new(CommittedLeaderId::new(1, node), 1)),
            Membership::new(
                vec![BTreeSet::from([node])],
                BTreeMap::from([
                    (node, BasicNode::new("node")),
                    (learner, BasicNode::new("learner")),
                ]),
            ),
        );
        rejected(&mut state.clone(), &command, Some(&mismatch));
        let response = state.apply_feature_activation_with_membership(&command, Some(&membership));
        assert_accepted(&response);
        assert_eq!(state.revision, 1);
        assert_eq!(state.write_schema_floor, 4);
        assert_eq!(
            state.apply_feature_activation_with_membership(&command, Some(&membership)),
            response
        );
    }

    #[test]
    fn feature_activation_is_revision_fenced_monotonic_and_idempotent() {
        let mut state = ControlState::new("cluster:test");
        let command = FeatureActivationCommand {
            principal_id: "principal:test".to_string(),
            command_id: CommandId { value: id(99) },
            issued_at_unix_ms: 42,
            expected_control_revision: 0,
            read_schema_floor: 2,
            write_schema_floor: 2,
            feature: "atomic-layout-mutation-v2".to_string(),
        };
        let response = state.apply_feature_activation(&command);
        assert_accepted(&response);
        assert_eq!(state.revision, 1);
        assert_eq!(state.read_schema_floor, 2);
        assert_eq!(state.write_schema_floor, 2);
        assert!(
            state
                .activated_features
                .contains("atomic-layout-mutation-v2")
        );
        assert_eq!(state.apply_feature_activation(&command), response);
        assert_eq!(state.revision, 1);

        let mut conflict = command.clone();
        conflict.feature = "different".to_string();
        assert!(matches!(
            state.apply_feature_activation(&conflict).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::CommandIdConflict
            }
        ));
        let stale_command = FeatureActivationCommand {
            command_id: CommandId { value: id(100) },
            expected_control_revision: 0,
            ..command.clone()
        };
        assert!(matches!(
            state.apply_feature_activation(&stale_command).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::RevisionConflict {
                    expected: 0,
                    current: 1
                }
            }
        ));
        let downgrade = FeatureActivationCommand {
            command_id: CommandId { value: id(101) },
            expected_control_revision: 1,
            read_schema_floor: 1,
            write_schema_floor: 1,
            ..command
        };
        assert!(matches!(
            state.apply_feature_activation(&downgrade).result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::InvalidTransition { .. }
            }
        ));
    }

    #[test]
    fn advanced_feature_floor_uses_v3_snapshot_and_rejects_invalid_floor() {
        let mut state = ControlState::new("cluster:test");
        state.read_schema_floor = 2;
        state.write_schema_floor = 2;
        state
            .activated_features
            .insert("atomic-layout-mutation-v2".to_string());
        let activation = FeatureActivationCommand {
            principal_id: "principal:test".to_string(),
            command_id: CommandId { value: id(200) },
            issued_at_unix_ms: 5,
            expected_control_revision: 0,
            read_schema_floor: 2,
            write_schema_floor: 2,
            feature: "atomic-layout-mutation-v2".to_string(),
        };
        let key = DedupKey {
            principal_id: activation.principal_id.clone(),
            command_id: activation.command_id.value,
        };
        let encoded_activation = crate::control_codec::encode_feature_activation(&activation);
        state.feature_dedup.insert(
            key,
            FeatureDedupRecord {
                fingerprint: sha2::Sha256::digest(&encoded_activation).into(),
                issued_at_unix_ms: activation.issued_at_unix_ms,
                command: activation.clone(),
                response: ControlResponse {
                    schema_version: CONTROL_SCHEMA_VERSION,
                    command_id: activation.command_id,
                    control_revision: 1,
                    workflow_status: ControlWorkflowStatus::Complete,
                    result: ControlCommandResult::Accepted {
                        payload: Vec::new(),
                    },
                },
            },
        );
        let encoded = state.encode_snapshot().unwrap();
        assert_eq!(&encoded[..8], SNAPSHOT_MAGIC);
        assert_eq!(ControlState::decode_snapshot(&encoded).unwrap(), state);

        let mut invalid = state;
        invalid.read_schema_floor = 3;
        invalid.write_schema_floor = 2;
        let encoded = invalid.encode_snapshot().unwrap();
        assert!(matches!(
            ControlState::decode_snapshot(&encoded),
            Err(StateCodecError::InvalidState(
                "control feature floors are inconsistent"
            ))
        ));
    }

    #[test]
    fn snapshot_rejects_truncation_trailing_bytes_and_future_schema() {
        let state = ControlState::new("cluster:test");
        let bytes = state.encode_snapshot().unwrap();
        assert_eq!(
            ControlState::decode_snapshot(&bytes[..bytes.len() - 1]),
            Err(StateCodecError::Truncated)
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            ControlState::decode_snapshot(&trailing),
            Err(StateCodecError::TrailingBytes)
        );
        let mut future_format = bytes.clone();
        future_format[8..10].copy_from_slice(&4_u16.to_be_bytes());
        assert_eq!(
            ControlState::decode_snapshot(&future_format),
            Err(StateCodecError::UnsupportedSnapshotFormat(4))
        );
        let mut future_codec = bytes.clone();
        future_codec[10..12].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            ControlState::decode_snapshot(&future_codec),
            Err(StateCodecError::UnsupportedCodec(2))
        );
        let mut future = bytes;
        future[12..14].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            ControlState::decode_snapshot(&future),
            Err(StateCodecError::UnsupportedSchema(2))
        );
    }

    #[test]
    fn incomplete_workflows_survive_pruning_and_complete_idempotently() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let original = command(
            4,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 3,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: Some(launch_spec()),
            },
        );
        assert_eq!(
            state.apply(&original).workflow_status,
            ControlWorkflowStatus::Pending
        );
        assert_accepted(&state.apply(&command(
            5,
            ControlCommandRequest::PruneDedup {
                completed_before_unix_ms: 100,
            },
        )));
        assert_eq!(
            state.apply(&original).workflow_status,
            ControlWorkflowStatus::Pending
        );
        let completion = command(
            6,
            ControlCommandRequest::CompleteWorkflow {
                original_command_id: original.command_id.clone(),
                response: vec![1, 2, 3],
            },
        );
        assert_accepted(&state.apply(&completion));
        assert_accepted(&state.apply(&completion));
        let replay = state.apply(&original);
        assert_eq!(replay.workflow_status, ControlWorkflowStatus::Complete);
        assert_eq!(
            replay.result,
            ControlCommandResult::Accepted {
                payload: vec![1, 2, 3]
            }
        );
    }

    #[test]
    fn assignment_without_durable_launch_spec_is_rejected_atomically() {
        let mut state = ControlState::new("cluster:test");
        setup_pane(&mut state);
        let before = state.clone();
        let response = state.apply(&command(
            4,
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(30) },
                expected_revision: 3,
                expected_generation: 0,
                assignment: assignment(1),
                launch_spec: None,
            },
        ));
        assert!(matches!(
            response.result,
            ControlCommandResult::Rejected {
                error: ControlCommandError::InvalidTransition { .. }
            }
        ));
        assert_eq!(state.revision, before.revision);
        assert_eq!(state.panes, before.panes);
    }

    #[test]
    fn execution_identity_and_generation_are_independent_from_local_runtime_ids() {
        let logical_pane = LogicalPaneId { value: id(30) };
        let first = assignment(1);
        let second = assignment(2);
        let local_session_id = id(900);
        let local_pane_id = id(901);
        assert_ne!(logical_pane.value, first.execution_id.value);
        assert_ne!(first.execution_id.value, local_session_id);
        assert_ne!(first.execution_id.value, local_pane_id);
        assert_ne!(first.execution_id, second.execution_id);
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
    }

    fn assignment(generation: u64) -> ExecutionAssignment {
        ExecutionAssignment {
            node_id: "node:worker".to_string(),
            generation,
            execution_id: ExecutionId {
                value: id(40 + u128::from(generation)),
            },
        }
    }

    fn assert_accepted(response: &ControlResponse) {
        assert!(matches!(
            response.result,
            ControlCommandResult::Accepted { .. }
        ));
    }
}
