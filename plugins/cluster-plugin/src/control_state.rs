use crate::control_codec::{FeatureActivationCommand, request_fingerprint};
#[path = "control_state_codec.rs"]
mod control_state_codec;
use bmux_cluster_plugin_api::cluster_types::{
    ClusterMember, ClusterMemberState, ControlCommand, ControlCommandError, ControlCommandRequest,
    ControlCommandResult, ControlReadConsistency, ControlResourceKind, ControlResponse,
    ControlStateView, ControlWorkflowStatus, LogicalPaneRecord, LogicalWindowRecord,
    PaneAvailability, PendingWorkflow, WorkspaceId, WorkspaceRecord,
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
pub struct ControlState {
    pub schema_version: u16,
    pub cluster_id: String,
    pub revision: u64,
    pub read_schema_floor: u16,
    pub write_schema_floor: u16,
    pub activated_features: BTreeSet<String>,
    pub members: BTreeMap<String, ClusterMember>,
    pub workspaces: BTreeMap<uuid::Uuid, WorkspaceRecord>,
    pub windows: BTreeMap<uuid::Uuid, LogicalWindowRecord>,
    pub panes: BTreeMap<uuid::Uuid, LogicalPaneRecord>,
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
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            dedup: BTreeMap::new(),
            feature_dedup: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn to_view(&self, consistency: ControlReadConsistency) -> ControlStateView {
        ControlStateView {
            schema_version: self.schema_version,
            cluster_id: self.cluster_id.clone(),
            revision: self.revision,
            members: self.members.values().cloned().collect(),
            workspaces: self.workspaces.values().cloned().collect(),
            windows: self.windows.values().cloned().collect(),
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

    pub fn apply_feature_activation(
        &mut self,
        command: &FeatureActivationCommand,
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
        let result = self.validate_feature_activation(command);
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
                if let Some(existing) = self.members.get(&member.node_id) {
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
            ControlCommandRequest::PutWindow {
                window,
                expected_workspace_revision,
            } => {
                let workspace = workspace_mut(self, &window.workspace_id)?;
                require_revision(*expected_workspace_revision, workspace.revision)?;
                let changed = self.windows.get(&window.window_id.value) != Some(window);
                self.windows.insert(window.window_id.value, window.clone());
                Ok(ApplyOutcome::complete(changed))
            }
            ControlCommandRequest::RemoveWindow {
                window_id,
                expected_workspace_revision,
            } => {
                let window = self.windows.get(&window_id.value).ok_or_else(|| {
                    not_found(ControlResourceKind::Window, window_id.value.to_string())
                })?;
                let workspace_id = window.workspace_id.clone();
                require_revision(
                    *expected_workspace_revision,
                    workspace(self, &workspace_id)?.revision,
                )?;
                if self
                    .panes
                    .values()
                    .any(|pane| pane.window_id.value == window_id.value)
                {
                    return Err(invalid_transition("window still contains logical panes"));
                }
                self.windows.remove(&window_id.value);
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
            ControlCommandRequest::PutWindow { window, .. } => {
                if let Some(stored) = self.windows.get_mut(&window.window_id.value) {
                    stored.revision = revision;
                }
                if let Some(workspace) = self.workspaces.get_mut(&window.workspace_id.value) {
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
            | ControlCommandRequest::RemoveWindow { .. }
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
    let window = state.windows.get(&pane.window_id.value).ok_or_else(|| {
        ControlCommandError::InvalidReference {
            resource: ControlResourceKind::Window,
            id: pane.window_id.value.to_string(),
        }
    })?;
    if window.workspace_id != pane.workspace_id {
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
        CommandId, ExecutionAssignment, ExecutionId, LogicalPaneId, LogicalWindowId,
        PaneAvailability, PaneRestartPolicy, PlacementIntent, WorkerLaunchSpec,
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
            ControlCommandRequest::PutWindow {
                window: LogicalWindowRecord {
                    window_id: LogicalWindowId { value: id(20) },
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
                    window_id: LogicalWindowId { value: id(20) },
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
                    window_id: LogicalWindowId { value: id(20) },
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
        assert_eq!(pane.window_id.value, id(20));
        assert!(state.windows.contains_key(&id(20)));
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

    #[test]
    fn membership_updates_reject_stale_and_same_timestamp_conflicts() {
        let mut state = ControlState::new("cluster:test");
        let identity = crate::membership::NodeIdentity::new_for_test(1);
        let cluster_id = "cluster:00000000-0000-0000-0000-000000000001"
            .parse::<crate::membership::ClusterId>()
            .unwrap();
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
        member.cluster_id = "cluster:test".to_string();
        assert_accepted(&state.apply(&command(
            50,
            ControlCommandRequest::UpsertMember {
                member: member.clone(),
            },
        )));

        let mut older = member.clone();
        older.updated_at_unix_ms -= 1;
        assert!(matches!(
            state
                .apply(&command(
                    51,
                    ControlCommandRequest::UpsertMember { member: older }
                ))
                .result,
            ControlCommandResult::Rejected { .. }
        ));
        let mut conflicting = member;
        conflicting.endpoint = Some("different".to_string());
        assert!(matches!(
            state
                .apply(&command(
                    52,
                    ControlCommandRequest::UpsertMember {
                        member: conflicting
                    }
                ))
                .result,
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
        let stale = FeatureActivationCommand {
            command_id: CommandId { value: id(100) },
            expected_control_revision: 0,
            ..command.clone()
        };
        assert!(matches!(
            state.apply_feature_activation(&stale).result,
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
