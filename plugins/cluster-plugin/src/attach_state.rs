//! Authenticated ingress facade for attach-time worker reads.

use crate::endpoint::EndpointDispatchClient;
use bmux_cluster_plugin_api::cluster_types::{
    AttachLayout, AttachLayoutRect, ControlServiceError, ExecutionId, WorkerOutput,
    WorkerServiceError, WorkerTerminalSnapshot, WorkspaceId,
};
use bmux_plugin::ServiceCaller;
use std::sync::Arc;

pub struct AttachStateServiceHandle<C> {
    caller: Arc<C>,
    local_node_id: crate::membership::NodeId,
    control: Arc<crate::consensus_network::ControlServiceHandle<C>>,
}

impl<C> AttachStateServiceHandle<C> {
    #[must_use]
    pub const fn new(
        caller: Arc<C>,
        local_node_id: crate::membership::NodeId,
        control: Arc<crate::consensus_network::ControlServiceHandle<C>>,
    ) -> Self {
        Self {
            caller,
            local_node_id,
            control,
        }
    }
}

impl<C> AttachStateServiceHandle<C>
where
    C: ServiceCaller + crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    async fn worker_authority(
        &self,
        workspace_id: &WorkspaceId,
        execution_id: &ExecutionId,
        generation: u64,
        command_id: &bmux_cluster_plugin_api::cluster_types::CommandId,
        principal_id: String,
        operation_class: bmux_cluster_plugin_api::cluster_types::WorkerOperationClass,
    ) -> Result<
        (
            String,
            bmux_cluster_plugin_api::cluster_types::WorkerAuthority,
        ),
        WorkerServiceError,
    > {
        let endpoint = self
            .resolve_worker(workspace_id, execution_id, generation)
            .await?;
        let view = bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(self.control.as_ref()).await.map_err(|error| control_error(&error))?;
        let pane = view
            .panes
            .iter()
            .find(|pane| {
                pane.workspace_id == *workspace_id
                    && pane.execution.as_ref().is_some_and(|assignment| {
                        assignment.execution_id == *execution_id
                            && assignment.generation == generation
                    })
            })
            .ok_or_else(|| WorkerServiceError::NotFound {
                execution_id: execution_id.clone(),
            })?;
        let assignment = pane.execution.as_ref().expect("matched assignment");
        let identity = crate::membership::load_or_create_node_identity(self.caller.as_ref())
            .map_err(|reason| WorkerServiceError::Unavailable { reason })?;
        let mut authority = bmux_cluster_plugin_api::cluster_types::WorkerAuthority {
            cluster_id: view.cluster_id,
            workspace_id: workspace_id.clone(),
            pane_id: pane.pane_id.clone(),
            execution_id: execution_id.clone(),
            generation,
            control_term: self
                .control
                .active()
                .map_err(|error| control_error(&error))?
                .current_term(),
            lease_sequence: u64::from_be_bytes(
                command_id.value.as_bytes()[8..]
                    .try_into()
                    .expect("UUID suffix length"),
            ),
            operation_class,
            principal_id,
            issuer_node_id: identity.node_id().to_string(),
            audience_node_id: assignment.node_id.clone(),
            lease_id: command_id.value,
            lease_issued_at_unix_ms: crate::now_unix_ms(),
            lease_duration_ms: 5_000,
            lease_signature: Vec::new(),
        };
        authority.lease_signature = identity.sign(
            &crate::worker_runtime::canonical_unsigned_authority(&authority)?,
        );
        Ok((endpoint, authority))
    }

    async fn resolve_worker(
        &self,
        workspace_id: &WorkspaceId,
        execution_id: &ExecutionId,
        generation: u64,
    ) -> Result<String, WorkerServiceError> {
        let view = bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(
            self.control.as_ref(),
        )
        .await
        .map_err(|error| control_error(&error))?;
        let pane = view
            .panes
            .iter()
            .find(|pane| {
                pane.workspace_id == *workspace_id
                    && pane.execution.as_ref().is_some_and(|assignment| {
                        assignment.execution_id == *execution_id
                            && assignment.generation == generation
                    })
            })
            .ok_or_else(|| WorkerServiceError::NotFound {
                execution_id: execution_id.clone(),
            })?;
        let assignment = pane
            .execution
            .as_ref()
            .expect("matched execution assignment");
        let member = view
            .members
            .iter()
            .find(|member| member.node_id == assignment.node_id)
            .filter(|member| {
                member.state == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
                    && member.capabilities.worker
            })
            .ok_or_else(|| WorkerServiceError::Unavailable {
                reason: format!(
                    "assigned node {} is not an active worker",
                    assignment.node_id
                ),
            })?;
        crate::membership::verify_membership_credential(member, crate::now_unix_ms())
            .map_err(|reason| WorkerServiceError::Unavailable { reason })?;
        let endpoint = member
            .endpoint
            .as_deref()
            .filter(|endpoint| !endpoint.trim().is_empty())
            .ok_or_else(|| WorkerServiceError::Unavailable {
                reason: format!("assigned worker {} has no endpoint", member.node_id),
            })?;
        crate::endpoint::mutually_authenticate_endpoint(
            self.caller.as_ref(),
            endpoint,
            &self.local_node_id.to_string(),
            &member.node_id,
        )
        .await
        .map_err(|error| WorkerServiceError::Unavailable {
            reason: format!("assigned worker authentication failed: {error}"),
        })?;
        Ok(endpoint.to_string())
    }
}

impl<C> bmux_cluster_plugin_api::cluster_attach_command::ClusterAttachCommandService
    for AttachStateServiceHandle<C>
where
    C: ServiceCaller + crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn action<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_types::AttachActionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::ControlResponse,
                        ControlServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let control_request = match request.action.as_str() {
                "close" => {
                    let pane_id = parse_action_uuid(&request.arguments, 0, "pane ID")?;
                    let view = bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(self.control.as_ref()).await?;
                    let pane = view
                        .panes
                        .iter()
                        .find(|pane| {
                            pane.workspace_id == request.workspace_id
                                && pane.pane_id.value == pane_id
                        })
                        .ok_or_else(|| ControlServiceError::Internal {
                            reason: "logical pane was not found".to_string(),
                        })?;
                    bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::RemovePane {
                        pane_id: pane.pane_id.clone(),
                        expected_revision: pane.revision,
                        expected_generation: pane
                            .execution
                            .as_ref()
                            .map(|assignment| assignment.generation),
                    }
                }
                "rename" => {
                    let name = request
                        .arguments
                        .first()
                        .cloned()
                        .filter(|name| !name.trim().is_empty());
                    let view = bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(self.control.as_ref()).await?;
                    let workspace = view
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == request.workspace_id)
                        .ok_or_else(|| ControlServiceError::Internal {
                            reason: "logical workspace was not found".to_string(),
                        })?;
                    bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::RenameWorkspace {
                        workspace_id: request.workspace_id,
                        expected_revision: workspace.revision,
                        name,
                    }
                }
                action => {
                    return Err(ControlServiceError::Internal {
                        reason: format!("unsupported logical attach action '{action}'"),
                    });
                }
            };
            bmux_cluster_plugin_api::cluster_control_command::ClusterControlCommandService::mutate(
                self.control.as_ref(),
                bmux_cluster_plugin_api::cluster_types::ControlCommand {
                    schema_version: crate::control_state::CONTROL_SCHEMA_VERSION,
                    principal_id: request.principal_id,
                    command_id: request.command_id,
                    issued_at_unix_ms: crate::now_unix_ms(),
                    request: control_request,
                },
            )
            .await
        })
    }

    fn input<'a>(
        &'a self,
        workspace_id: WorkspaceId,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        execution_id: ExecutionId,
        generation: u64,
        principal_id: String,
        data: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let (endpoint, authority) = self
                .worker_authority(
                    &workspace_id,
                    &execution_id,
                    generation,
                    &command_id,
                    principal_id,
                    bmux_cluster_plugin_api::cluster_types::WorkerOperationClass::Interactive,
                )
                .await?;
            let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
            bmux_cluster_plugin_api::cluster_worker_command::client::input(
                &mut remote,
                command_id,
                authority,
                data,
            )
            .await
            .map_err(dispatch_error)?
        })
    }

    fn resize<'a>(
        &'a self,
        request: bmux_cluster_plugin_api::cluster_types::AttachResizeRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let (endpoint, authority) = self
                .worker_authority(
                    &request.workspace_id,
                    &request.execution_id,
                    request.generation,
                    &request.command_id,
                    request.principal_id,
                    bmux_cluster_plugin_api::cluster_types::WorkerOperationClass::Interactive,
                )
                .await?;
            let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
            bmux_cluster_plugin_api::cluster_worker_command::client::resize(
                &mut remote,
                request.command_id,
                authority,
                request.cols,
                request.rows,
            )
            .await
            .map_err(dispatch_error)?
        })
    }
}

impl<C> bmux_cluster_plugin_api::cluster_attach_state::ClusterAttachStateService
    for AttachStateServiceHandle<C>
where
    C: ServiceCaller + crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn layout<'a>(
        &'a self,
        workspace_id: WorkspaceId,
        window_id: Option<bmux_cluster_plugin_api::cluster_types::LogicalWindowId>,
        columns: u16,
        rows: u16,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<AttachLayout, ControlServiceError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let view = bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(self.control.as_ref()).await?;
            let windows = view
                .windows
                .iter()
                .filter(|window| window.workspace_id == workspace_id)
                .collect::<Vec<_>>();
            let selected = select_layout_window(&windows, window_id.as_ref(), &workspace_id)?;
            let panes = view
                .panes
                .iter()
                .filter(|pane| {
                    pane.workspace_id == workspace_id && pane.window_id == selected.window_id
                })
                .cloned()
                .collect::<Vec<_>>();
            if panes.is_empty() {
                return Err(ControlServiceError::Internal {
                    reason: "logical window has no panes".to_string(),
                });
            }
            let mut rects = Vec::new();
            if columns == 0 || rows == 0 {
                return Err(ControlServiceError::Internal {
                    reason: "logical layout viewport must be non-zero".to_string(),
                });
            }
            if selected.layout.is_empty() && panes.len() == 1 {
                rects.push(AttachLayoutRect {
                    pane_id: panes[0].pane_id.clone(),
                    x: 0,
                    y: 0,
                    width: columns,
                    height: rows,
                });
            } else {
                let root = decode_layout(selected)?;
                project_layout(
                    &root,
                    LayoutRect {
                        x: 0,
                        y: 0,
                        width: columns,
                        height: rows,
                    },
                    &mut rects,
                )?;
            }
            validate_projected_layout(&panes, &rects)?;
            Ok(AttachLayout {
                workspace_id,
                window_id: selected.window_id.clone(),
                control_revision: view.revision,
                panes,
                rects,
            })
        })
    }

    fn snapshot<'a>(
        &'a self,
        workspace_id: WorkspaceId,
        execution_id: ExecutionId,
        generation: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerTerminalSnapshot, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let endpoint = self
                .resolve_worker(&workspace_id, &execution_id, generation)
                .await?;
            let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
            bmux_cluster_plugin_api::cluster_worker_state::client::snapshot(
                &mut remote,
                execution_id,
                generation,
            )
            .await
            .map_err(dispatch_error)?
        })
    }

    fn output<'a>(
        &'a self,
        workspace_id: WorkspaceId,
        execution_id: ExecutionId,
        generation: u64,
        cursor: u64,
        max_bytes: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<WorkerOutput, WorkerServiceError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let endpoint = self
                .resolve_worker(&workspace_id, &execution_id, generation)
                .await?;
            let mut remote = EndpointDispatchClient::new(self.caller.as_ref(), endpoint);
            bmux_cluster_plugin_api::cluster_worker_state::client::output(
                &mut remote,
                execution_id,
                generation,
                cursor,
                max_bytes,
            )
            .await
            .map_err(dispatch_error)?
        })
    }
}

#[derive(Clone, Copy)]
struct LayoutRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

fn decode_layout(
    window: &bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord,
) -> Result<bmux_attach_layout_protocol::PaneLayoutNode, ControlServiceError> {
    if window.layout_schema_version != 1 {
        return Err(ControlServiceError::Internal {
            reason: format!(
                "unsupported logical layout schema {}",
                window.layout_schema_version
            ),
        });
    }
    bmux_codec::from_bytes(&window.layout).map_err(|error| ControlServiceError::Internal {
        reason: format!("logical layout is malformed: {error}"),
    })
}

fn project_layout(
    node: &bmux_attach_layout_protocol::PaneLayoutNode,
    rect: LayoutRect,
    output: &mut Vec<AttachLayoutRect>,
) -> Result<(), ControlServiceError> {
    match node {
        bmux_attach_layout_protocol::PaneLayoutNode::Leaf { pane_id } => {
            output.push(AttachLayoutRect {
                pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId { value: *pane_id },
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            });
        }
        bmux_attach_layout_protocol::PaneLayoutNode::Split {
            direction,
            ratio_percent,
            first,
            second,
        } => {
            if !(1..100).contains(ratio_percent) {
                return Err(ControlServiceError::Internal {
                    reason: "logical split ratio must be within 1..99".to_string(),
                });
            }
            let ratio = u32::from(*ratio_percent);
            match direction {
                bmux_attach_layout_protocol::PaneSplitDirection::Vertical => {
                    let first_width =
                        u16::try_from(u32::from(rect.width).saturating_mul(ratio) / 100)
                            .unwrap_or(rect.width);
                    let second_width = rect.width.saturating_sub(first_width);
                    if first_width == 0 || second_width == 0 {
                        return Err(ControlServiceError::Internal {
                            reason: "logical split has no renderable width".to_string(),
                        });
                    }
                    project_layout(
                        first,
                        LayoutRect {
                            width: first_width,
                            ..rect
                        },
                        output,
                    )?;
                    project_layout(
                        second,
                        LayoutRect {
                            x: rect.x.saturating_add(first_width),
                            width: second_width,
                            ..rect
                        },
                        output,
                    )?;
                }
                bmux_attach_layout_protocol::PaneSplitDirection::Horizontal => {
                    let first_height =
                        u16::try_from(u32::from(rect.height).saturating_mul(ratio) / 100)
                            .unwrap_or(rect.height);
                    let second_height = rect.height.saturating_sub(first_height);
                    if first_height == 0 || second_height == 0 {
                        return Err(ControlServiceError::Internal {
                            reason: "logical split has no renderable height".to_string(),
                        });
                    }
                    project_layout(
                        first,
                        LayoutRect {
                            height: first_height,
                            ..rect
                        },
                        output,
                    )?;
                    project_layout(
                        second,
                        LayoutRect {
                            y: rect.y.saturating_add(first_height),
                            height: second_height,
                            ..rect
                        },
                        output,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn select_layout_window<'a>(
    windows: &[&'a bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord],
    requested: Option<&bmux_cluster_plugin_api::cluster_types::LogicalWindowId>,
    workspace_id: &WorkspaceId,
) -> Result<&'a bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord, ControlServiceError> {
    requested.map_or_else(
        || {
            windows
                .first()
                .copied()
                .ok_or_else(|| ControlServiceError::Internal {
                    reason: "logical workspace has no windows".to_string(),
                })
        },
        |requested| {
            windows
                .iter()
                .copied()
                .find(|window| window.window_id == *requested)
                .ok_or_else(|| ControlServiceError::Internal {
                    reason: format!(
                        "requested logical window {} was not found in workspace {}",
                        requested.value, workspace_id.value
                    ),
                })
        },
    )
}

fn validate_projected_layout(
    panes: &[bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord],
    rects: &[AttachLayoutRect],
) -> Result<(), ControlServiceError> {
    let pane_ids = panes
        .iter()
        .map(|pane| pane.pane_id.value)
        .collect::<std::collections::BTreeSet<_>>();
    let rect_ids = rects
        .iter()
        .map(|rect| rect.pane_id.value)
        .collect::<std::collections::BTreeSet<_>>();
    if rect_ids.len() != rects.len() || rect_ids != pane_ids {
        return Err(ControlServiceError::Internal {
            reason: "logical layout must reference every selected-window pane exactly once"
                .to_string(),
        });
    }
    Ok(())
}

fn parse_action_uuid(
    arguments: &[String],
    index: usize,
    name: &str,
) -> Result<uuid::Uuid, ControlServiceError> {
    arguments
        .get(index)
        .ok_or_else(|| ControlServiceError::Internal {
            reason: format!("attach action requires {name}"),
        })?
        .parse()
        .map_err(|error| ControlServiceError::Internal {
            reason: format!("attach action {name} is invalid: {error}"),
        })
}

fn control_error(error: &ControlServiceError) -> WorkerServiceError {
    WorkerServiceError::Unavailable {
        reason: format!("linearizable attach routing state unavailable: {error:?}"),
    }
}

fn dispatch_error(error: impl std::fmt::Display) -> WorkerServiceError {
    WorkerServiceError::Unavailable {
        reason: format!("worker attach dispatch failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_nested_logical_layout_exactly() {
        use bmux_attach_layout_protocol::{PaneLayoutNode, PaneSplitDirection};
        let first = uuid::Uuid::from_u128(1);
        let second = uuid::Uuid::from_u128(2);
        let third = uuid::Uuid::from_u128(3);
        let layout = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio_percent: 40,
            first: Box::new(PaneLayoutNode::Leaf { pane_id: first }),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Horizontal,
                ratio_percent: 50,
                first: Box::new(PaneLayoutNode::Leaf { pane_id: second }),
                second: Box::new(PaneLayoutNode::Leaf { pane_id: third }),
            }),
        };
        let mut rects = Vec::new();
        project_layout(
            &layout,
            LayoutRect {
                x: 0,
                y: 0,
                width: 100,
                height: 40,
            },
            &mut rects,
        )
        .unwrap();
        assert_eq!(rects.len(), 3);
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (0, 0, 40, 40)
        );
        assert_eq!(
            (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
            (40, 0, 60, 20)
        );
        assert_eq!(
            (rects[2].x, rects[2].y, rects[2].width, rects[2].height),
            (40, 20, 60, 20)
        );
    }

    #[test]
    fn explicit_unknown_window_fails_instead_of_falling_back() {
        let workspace_id = WorkspaceId {
            value: uuid::Uuid::from_u128(1),
        };
        let window = bmux_cluster_plugin_api::cluster_types::LogicalWindowRecord {
            window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                value: uuid::Uuid::from_u128(2),
            },
            workspace_id: workspace_id.clone(),
            name: None,
            layout_schema_version: 1,
            layout: Vec::new(),
            revision: 1,
        };
        let windows = [&window];
        assert_eq!(
            select_layout_window(&windows, None, &workspace_id)
                .unwrap()
                .window_id,
            window.window_id
        );
        assert!(
            select_layout_window(
                &windows,
                Some(&bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                    value: uuid::Uuid::from_u128(3),
                }),
                &workspace_id,
            )
            .is_err()
        );
    }

    #[test]
    fn projected_layout_requires_exactly_one_rect_for_every_pane() {
        let pane = |id: u128| bmux_cluster_plugin_api::cluster_types::LogicalPaneRecord {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: uuid::Uuid::from_u128(id),
            },
            workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                value: uuid::Uuid::nil(),
            },
            window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                value: uuid::Uuid::nil(),
            },
            name: None,
            restart_policy: bmux_cluster_plugin_api::cluster_types::PaneRestartPolicy::Manual,
            placement: bmux_cluster_plugin_api::cluster_types::PlacementIntent {
                explicit_node_id: None,
                required_labels: Vec::new(),
                preferred_labels: Vec::new(),
            },
            availability: bmux_cluster_plugin_api::cluster_types::PaneAvailability::Unavailable,
            availability_reason: None,
            execution: None,
            revision: 1,
        };
        let panes = vec![pane(1), pane(2)];
        let rect = |id: u128| AttachLayoutRect {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: uuid::Uuid::from_u128(id),
            },
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(validate_projected_layout(&panes, &[rect(1), rect(2)]).is_ok());
        assert!(validate_projected_layout(&panes, &[rect(1)]).is_err());
        assert!(validate_projected_layout(&panes, &[rect(1), rect(1)]).is_err());
        assert!(validate_projected_layout(&panes, &[rect(1), rect(3)]).is_err());
    }

    #[test]
    fn rejects_invalid_split_ratios() {
        let layout = bmux_attach_layout_protocol::PaneLayoutNode::Split {
            direction: bmux_attach_layout_protocol::PaneSplitDirection::Vertical,
            ratio_percent: 100,
            first: Box::new(bmux_attach_layout_protocol::PaneLayoutNode::Leaf {
                pane_id: uuid::Uuid::from_u128(1),
            }),
            second: Box::new(bmux_attach_layout_protocol::PaneLayoutNode::Leaf {
                pane_id: uuid::Uuid::from_u128(2),
            }),
        };
        assert!(
            project_layout(
                &layout,
                LayoutRect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 24
                },
                &mut Vec::new()
            )
            .is_err()
        );
    }
}
