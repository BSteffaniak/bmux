//! Leader-side reconciliation of durable execution-assignment workflows.

use crate::consensus_network::ConsensusNodeRegistry;
use crate::endpoint::EndpointDispatchClient;
use crate::membership::NodeIdentity;
use crate::worker_runtime::canonical_unsigned_authority;
use bmux_cluster_plugin_api::cluster_types::{
    ClusterMember, ClusterMemberState, CommandId, ControlCommand, ControlCommandRequest,
    ControlCommandResult, ControlServiceError, ControlStateView, ExecutionAssignment,
    LogicalPaneId, PaneAvailability, WorkerAuthority, WorkerLaunchResult, WorkerOperationClass,
    WorkerServiceError, WorkspaceId,
};
#[cfg(test)]
use bmux_cluster_plugin_api::cluster_worker_command::ClusterWorkerCommandService;
#[cfg(test)]
use bmux_cluster_plugin_api::cluster_worker_state::ClusterWorkerStateService;
use bmux_plugin::ServiceCaller;
use std::sync::Arc;
use std::time::Duration;

const RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RECONCILE_BACKOFF: Duration = Duration::from_secs(5);
const LEASE_DURATION_MS: u64 = 5_000;

pub async fn run<C>(caller: Arc<C>, identity: NodeIdentity, nodes: ConsensusNodeRegistry)
where
    C: ServiceCaller + crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    let mut delay = RECONCILE_INTERVAL;
    loop {
        match reconcile_once(caller.as_ref(), &identity, &nodes).await {
            Ok(_) => delay = RECONCILE_INTERVAL,
            Err(error) => {
                tracing::debug!(%error, retry_ms = delay.as_millis(), "cluster pending-workflow reconciliation deferred");
                delay = delay.saturating_mul(2).min(MAX_RECONCILE_BACKOFF);
            }
        }
        tokio::time::sleep(delay).await;
    }
}

pub trait WorkerWorkflowClient: Send + Sync {
    fn get<'a>(
        &'a self,
        endpoint: &'a str,
        execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerQueryResult,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    fn launch<'a>(
        &'a self,
        endpoint: &'a str,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    >;
}

struct EndpointWorkerClient<'a, C>(&'a C);

impl<C> WorkerWorkflowClient for EndpointWorkerClient<'_, C>
where
    C: ServiceCaller + Sync,
{
    fn get<'a>(
        &'a self,
        endpoint: &'a str,
        execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerQueryResult,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut remote = EndpointDispatchClient::new(self.0, endpoint);
            bmux_cluster_plugin_api::cluster_worker_state::client::get(&mut remote, execution_id)
                .await
                .map_err(|error| WorkerServiceError::Unavailable {
                    reason: format!("worker execution query failed: {error}"),
                })?
        })
    }

    fn launch<'a>(
        &'a self,
        endpoint: &'a str,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut remote = EndpointDispatchClient::new(self.0, endpoint);
            bmux_cluster_plugin_api::cluster_worker_command::client::launch(
                &mut remote,
                command_id,
                authority,
                spec,
            )
            .await
            .map_err(|error| WorkerServiceError::Unavailable {
                reason: format!("worker launch dispatch failed: {error}"),
            })?
        })
    }
}

#[cfg(test)]
pub(crate) struct TestWorkerClient<R, V, C> {
    endpoint: String,
    service: Arc<crate::worker_runtime::WorkerServiceHandle<R, V, C>>,
    launch_gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

#[cfg(test)]
impl<R, V, C> Clone for TestWorkerClient<R, V, C> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            service: self.service.clone(),
            launch_gate: self.launch_gate.clone(),
        }
    }
}

#[cfg(test)]
impl<R, V, C> TestWorkerClient<R, V, C> {
    pub(crate) fn new(
        endpoint: impl Into<String>,
        service: crate::worker_runtime::WorkerServiceHandle<R, V, C>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            service: Arc::new(service),
            launch_gate: None,
        }
    }

    pub(crate) fn with_launch_gate(
        mut self,
        launched: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) -> Self {
        self.launch_gate = Some((launched, release));
        self
    }
}

#[cfg(test)]
impl<R, V, C> WorkerWorkflowClient for TestWorkerClient<R, V, C>
where
    R: crate::worker_runtime::WorkerPaneRuntime + 'static,
    V: crate::worker_runtime::WorkerLeaseVerifier + 'static,
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn get<'a>(
        &'a self,
        endpoint: &'a str,
        execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerQueryResult,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if endpoint != self.endpoint {
                return Err(WorkerServiceError::Unavailable {
                    reason: "test worker endpoint mismatch".to_string(),
                });
            }
            self.service.get(execution_id).await
        })
    }

    fn launch<'a>(
        &'a self,
        endpoint: &'a str,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if endpoint != self.endpoint {
                return Err(WorkerServiceError::Unavailable {
                    reason: "test worker endpoint mismatch".to_string(),
                });
            }
            let result = self.service.launch(command_id, authority, spec).await;
            if result.is_ok()
                && let Some((launched, release)) = &self.launch_gate
            {
                launched.notify_one();
                release.notified().await;
            }
            result
        })
    }
}

async fn reconcile_once<C>(
    caller: &C,
    identity: &NodeIdentity,
    nodes: &ConsensusNodeRegistry,
) -> Result<usize, String>
where
    C: ServiceCaller + crate::ClusterRuntimeOps + Send + Sync,
{
    let node = nodes.get(*identity.node_id())?;
    let view = match node.read_linearizable_view().await {
        Ok(view) => view,
        Err(
            ControlServiceError::NotLeader { .. } | ControlServiceError::QuorumUnavailable { .. },
        ) => {
            return Ok(0);
        }
        Err(error) => return Err(format!("pending-workflow read failed: {error:?}")),
    };
    let worker = EndpointWorkerClient(caller);
    reconcile_pending(&worker, identity, &node, view).await
}

async fn reconcile_pending<W: WorkerWorkflowClient>(
    worker: &W,
    identity: &NodeIdentity,
    node: &crate::consensus_runtime::ConsensusNode,
    view: ControlStateView,
) -> Result<usize, String> {
    let mut completed = 0;
    for pending in view.pending_workflows.clone() {
        let ControlCommandRequest::AssignExecution {
            pane_id,
            assignment,
            launch_spec,
            ..
        } = &pending.control_command.request
        else {
            continue;
        };
        let Some(spec) = launch_spec else {
            continue;
        };
        if reconcile_launch(
            worker,
            identity,
            node,
            &view,
            &pending.principal_id,
            &pending.control_command,
            pane_id,
            assignment,
            spec,
        )
        .await?
        {
            completed += 1;
        }
    }
    Ok(completed)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn reconcile_launch<W: WorkerWorkflowClient>(
    worker: &W,
    identity: &NodeIdentity,
    node: &crate::consensus_runtime::ConsensusNode,
    view: &ControlStateView,
    principal_id: &str,
    original: &ControlCommand,
    pane_id: &LogicalPaneId,
    assignment: &ExecutionAssignment,
    spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
) -> Result<bool, String> {
    let Some(pane) = view.panes.iter().find(|pane| pane.pane_id == *pane_id) else {
        return Ok(false);
    };
    if pane.execution.as_ref() != Some(assignment) {
        return Ok(false);
    }
    let member = active_worker(view, &assignment.node_id)?;
    if member.cluster_id != view.cluster_id {
        return Err(format!(
            "assigned worker {} belongs to a different cluster",
            assignment.node_id
        ));
    }
    crate::membership::verify_membership_credential(member, crate::now_unix_ms())?;
    let endpoint = member
        .endpoint
        .as_deref()
        .ok_or_else(|| format!("assigned worker {} has no endpoint", assignment.node_id))?;
    let issued_at = original.issued_at_unix_ms;
    let existing = worker
        .get(endpoint, assignment.execution_id.clone())
        .await
        .map_err(|error| worker_error(&error))?;
    let execution = match existing {
        bmux_cluster_plugin_api::cluster_types::WorkerQueryResult::Found { execution } => execution,
        bmux_cluster_plugin_api::cluster_types::WorkerQueryResult::Missing => {
            launch_execution(
                worker,
                identity,
                node,
                view,
                principal_id,
                original,
                pane_id,
                assignment,
                spec,
                endpoint,
                issued_at,
            )
            .await?
        }
    };
    if execution.authority.execution_id != assignment.execution_id
        || execution.authority.generation != assignment.generation
    {
        return Err("worker launch returned a different execution identity".to_string());
    }
    let availability = match execution.state {
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Ready => {
            PaneAvailability::Ready
        }
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Launching => {
            PaneAvailability::Pending
        }
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Exited => {
            PaneAvailability::Exited
        }
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Unavailable => {
            PaneAvailability::Unavailable
        }
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Quarantined => {
            PaneAvailability::Quarantined
        }
        bmux_cluster_plugin_api::cluster_types::WorkerExecutionState::Closed => {
            PaneAvailability::Failed
        }
    };
    if pane.availability != availability {
        let mut purpose = b"availability".to_vec();
        purpose.extend_from_slice(&pane.revision.to_be_bytes());
        purpose.push(availability_tag(availability));
        let availability_command = ControlCommand {
            schema_version: original.schema_version,
            principal_id: principal_id.to_string(),
            command_id: derived_command_id(original.command_id.value, &purpose),
            issued_at_unix_ms: issued_at,
            request: ControlCommandRequest::SetPaneAvailability {
                pane_id: pane_id.clone(),
                expected_revision: pane.revision,
                assignment: assignment.clone(),
                availability,
                reason: None,
            },
        };
        let availability_response = node
            .mutate(availability_command)
            .await
            .map_err(|error| control_error(&error))?;
        if availability_response.result
            != (ControlCommandResult::Accepted {
                payload: Vec::new(),
            })
        {
            return Err("availability mutation was not accepted".to_string());
        }
    }
    if availability == PaneAvailability::Pending {
        return Ok(false);
    }
    let payload = bmux_plugin_sdk::encode_service_message(&execution)
        .map_err(|error| format!("worker outcome encoding failed: {error}"))?;
    let complete = ControlCommand {
        schema_version: original.schema_version,
        principal_id: principal_id.to_string(),
        command_id: derived_command_id(original.command_id.value, b"complete"),
        issued_at_unix_ms: issued_at,
        request: ControlCommandRequest::CompleteWorkflow {
            original_command_id: original.command_id.clone(),
            response: payload,
        },
    };
    node.mutate(complete)
        .await
        .map_err(|error| control_error(&error))?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn launch_execution<W: WorkerWorkflowClient>(
    worker: &W,
    identity: &NodeIdentity,
    node: &crate::consensus_runtime::ConsensusNode,
    view: &ControlStateView,
    principal_id: &str,
    original: &ControlCommand,
    pane_id: &LogicalPaneId,
    assignment: &ExecutionAssignment,
    spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    endpoint: &str,
    issued_at: u64,
) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerExecution, String> {
    let pane = view
        .panes
        .iter()
        .find(|pane| pane.pane_id == *pane_id)
        .ok_or_else(|| "assigned pane disappeared during reconciliation".to_string())?;
    let mut authority = WorkerAuthority {
        cluster_id: view.cluster_id.clone(),
        workspace_id: WorkspaceId {
            value: pane.workspace_id.value,
        },
        pane_id: pane_id.clone(),
        execution_id: assignment.execution_id.clone(),
        generation: assignment.generation,
        control_term: node.current_term(),
        lease_sequence: u64::from_be_bytes(
            original.command_id.value.as_bytes()[8..]
                .try_into()
                .expect("UUID suffix is exactly eight bytes"),
        ),
        operation_class: WorkerOperationClass::Lifecycle,
        principal_id: principal_id.to_string(),
        issuer_node_id: identity.node_id().to_string(),
        audience_node_id: assignment.node_id.clone(),
        lease_id: original.command_id.value,
        lease_issued_at_unix_ms: issued_at,
        lease_duration_ms: LEASE_DURATION_MS,
        lease_signature: Vec::new(),
    };
    authority.lease_signature = identity.sign(
        &canonical_unsigned_authority(&authority)
            .map_err(|error| format!("worker authority encoding failed: {error:?}"))?,
    );
    let launch = worker
        .launch(
            endpoint,
            original.command_id.clone(),
            authority,
            spec.clone(),
        )
        .await
        .map_err(|error| worker_error(&error))?;
    Ok(match launch {
        WorkerLaunchResult::Ready { execution } | WorkerLaunchResult::Pending { execution } => {
            execution
        }
    })
}

fn active_worker<'a>(
    view: &'a ControlStateView,
    node_id: &str,
) -> Result<&'a ClusterMember, String> {
    view.members
        .iter()
        .find(|member| member.node_id == node_id)
        .filter(|member| member.state == ClusterMemberState::Active && member.capabilities.worker)
        .ok_or_else(|| format!("assigned node {node_id} is not an active worker"))
}

const fn availability_tag(availability: PaneAvailability) -> u8 {
    match availability {
        PaneAvailability::Pending => 0,
        PaneAvailability::Ready => 1,
        PaneAvailability::Suspect => 2,
        PaneAvailability::Unavailable => 3,
        PaneAvailability::Reconciling => 4,
        PaneAvailability::Replacing => 5,
        PaneAvailability::Exited => 6,
        PaneAvailability::Failed => 7,
        PaneAvailability::Quarantined => 8,
    }
}

fn derived_command_id(original: uuid::Uuid, purpose: &[u8]) -> CommandId {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.workflow-command.v1\0");
    digest.update(original.as_bytes());
    digest.update(purpose);
    let bytes: [u8; 32] = digest.finalize().into();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[..16]);
    CommandId {
        value: uuid::Uuid::from_bytes(id),
    }
}

fn worker_error(error: &WorkerServiceError) -> String {
    format!("worker launch rejected: {error:?}")
}

fn control_error(error: &ControlServiceError) -> String {
    format!("workflow control mutation failed: {error:?}")
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_signature_matches_worker_verifier_payload() {
        let identity = NodeIdentity::new_for_test(9);
        let mut authority = WorkerAuthority {
            cluster_id: "cluster:test".to_string(),
            workspace_id: WorkspaceId {
                value: uuid::Uuid::from_u128(1),
            },
            pane_id: LogicalPaneId {
                value: uuid::Uuid::from_u128(2),
            },
            execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
                value: uuid::Uuid::from_u128(3),
            },
            generation: 4,
            control_term: 5,
            lease_sequence: 6,
            operation_class: WorkerOperationClass::Lifecycle,
            principal_id: "principal:test".to_string(),
            issuer_node_id: identity.node_id().to_string(),
            audience_node_id: "node:worker".to_string(),
            lease_id: uuid::Uuid::from_u128(7),
            lease_issued_at_unix_ms: 8,
            lease_duration_ms: LEASE_DURATION_MS,
            lease_signature: Vec::new(),
        };
        let payload = canonical_unsigned_authority(&authority).unwrap();
        authority.lease_signature = identity.sign(&payload);
        assert!(
            identity
                .verify(&payload, &authority.lease_signature)
                .is_ok()
        );
        authority.generation += 1;
        let tampered = canonical_unsigned_authority(&authority).unwrap();
        assert!(
            identity
                .verify(&tampered, &authority.lease_signature)
                .is_err()
        );
    }

    #[test]
    fn derived_ids_are_stable_and_purpose_separated() {
        let original = uuid::Uuid::from_u128(7);
        assert_eq!(
            derived_command_id(original, b"complete"),
            derived_command_id(original, b"complete")
        );
        assert_ne!(
            derived_command_id(original, b"complete"),
            derived_command_id(original, b"availability")
        );
    }
}
