use crate::control_codec::request_fingerprint;
use bmux_cluster_plugin_api::cluster_types::{
    ClusterMember, ClusterMemberState, ControlCommand, ControlCommandError, ControlCommandRequest,
    ControlCommandResult, ControlResourceKind, ControlResponse, ControlWorkflowStatus,
    LogicalPaneRecord, LogicalWindowRecord, PaneAvailability, WorkspaceId, WorkspaceRecord,
};
use std::collections::BTreeMap;

const CONTROL_SCHEMA_VERSION: u16 = 1;

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
    response: ControlResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlState {
    pub schema_version: u16,
    pub cluster_id: String,
    pub revision: u64,
    pub members: BTreeMap<String, ClusterMember>,
    pub workspaces: BTreeMap<uuid::Uuid, WorkspaceRecord>,
    pub windows: BTreeMap<uuid::Uuid, LogicalWindowRecord>,
    pub panes: BTreeMap<uuid::Uuid, LogicalPaneRecord>,
    dedup: BTreeMap<DedupKey, DedupRecord>,
}

impl ControlState {
    #[must_use]
    pub fn new(cluster_id: impl Into<String>) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            cluster_id: cluster_id.into(),
            revision: 0,
            members: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            dedup: BTreeMap::new(),
        }
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
                response: response.clone(),
            },
        );
        *self = next;
        response
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
                if let Some(existing) = self.members.get(&member.node_id)
                    && matches!(
                        existing.state,
                        ClusterMemberState::Revoked | ClusterMemberState::Left
                    )
                    && member.state == ClusterMemberState::Active
                {
                    return Err(invalid_transition(
                        "inactive member cannot be reactivated by upsert",
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
            } => {
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
                let before = self.dedup.len();
                self.dedup.retain(|_, record| {
                    record.response.workflow_status == ControlWorkflowStatus::Pending
                        || record.issued_at_unix_ms >= *completed_before_unix_ms
                });
                Ok(ApplyOutcome::complete(self.dedup.len() != before))
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
        PaneAvailability, PaneRestartPolicy, PlacementIntent,
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
