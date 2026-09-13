//! Sequential group effects with explicit partial-failure reporting.
use super::{
    block_on_typed_dispatch, menu::MoveSelectionRequest, tabs_commands, workspaces_commands,
    workspaces_state,
};
use bmux_plugin::ServiceCallerDispatchClient;
use bmux_plugin_sdk::{ServiceResponse, prelude::NativeServiceContext};

pub fn execute(
    req: MoveSelectionRequest,
    context: &NativeServiceContext,
) -> Result<(), ServiceResponse> {
    if req.tabs.is_empty() || req.tabs.len() > 4096 {
        return Err(ServiceResponse::error(
            "invalid_selection",
            "expected 1–4096 tabs",
        ));
    }
    let mut client = ServiceCallerDispatchClient::new(context);
    let fail = |reason: String| ServiceResponse::error("group_action_failed", reason);
    let destination = if req.action == "new" {
        let ack = block_on_typed_dispatch(workspaces_commands::client::new_workspace(
            &mut client,
            None,
        ))
        .map_err(|e| fail(e.to_string()))?
        .map_err(|e| fail(format!("{e:?}")))?;
        ack.id
    } else {
        req.workspace
    };
    let moves = if matches!(req.action.as_str(), "left" | "right") {
        let snapshot = block_on_typed_dispatch(workspaces_state::client::get_workspace(
            &mut client,
            workspaces_state::WorkspaceSelector {
                id: Some(req.workspace),
                name: None,
            },
        ))
        .map_err(|e| fail(e.to_string()))?
        .map_err(|e| fail(format!("{e:?}")))?;
        if req.tabs.iter().any(|id| !snapshot.tab_ids.contains(id)) {
            return Err(fail("Selection no longer belongs to this workspace".into()));
        }
        reorder_plan(snapshot.tab_ids, &req.tabs, req.action == "left")
    } else {
        Vec::new()
    };
    let mut completed = 0;
    let mut errors = Vec::new();
    if matches!(req.action.as_str(), "left" | "right") {
        for (source, target) in moves {
            let placement = if req.action == "left" {
                tabs_commands::TabMovePlacement::Before
            } else {
                tabs_commands::TabMovePlacement::After
            };
            match block_on_typed_dispatch(tabs_commands::client::move_tab(
                &mut client,
                source,
                target,
                placement,
            )) {
                Ok(Ok(_)) => completed += 1,
                error => {
                    errors.push(format!("{source}: {error:?}"));
                    break;
                }
            }
        }
    } else {
        for id in req.tabs {
            let outcome = match req.action.as_str() {
                "close" => block_on_typed_dispatch(tabs_commands::client::kill_tab(
                    &mut client,
                    id.to_string(),
                    false,
                ))
                .map(|r| r.map(|_| ()).map_err(|e| format!("{e:?}"))),
                "move" | "new" => {
                    block_on_typed_dispatch(workspaces_commands::client::move_tab_to_workspace(
                        &mut client,
                        id,
                        workspaces_state::WorkspaceSelector {
                            id: Some(destination),
                            name: None,
                        },
                    ))
                    .map(|r| r.map(|_| ()).map_err(|e| format!("{e:?}")))
                }
                _ => return Err(fail("Unknown group action".into())),
            };
            match outcome {
                Ok(Ok(())) => completed += 1,
                error => errors.push(format!("{id}: {error:?}")),
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(fail(format!(
            "Completed {completed}; destination {}; failures: {}",
            destination,
            errors.join("; ")
        )))
    }
}

fn reorder_plan(
    order: Vec<uuid::Uuid>,
    selected: &[uuid::Uuid],
    left: bool,
) -> Vec<(uuid::Uuid, uuid::Uuid)> {
    let mut order = order;
    let mut moves = Vec::new();
    let indices: Vec<usize> = if left {
        (1..order.len()).collect()
    } else {
        (0..order.len().saturating_sub(1)).rev().collect()
    };
    for index in indices {
        let neighbor = if left { index - 1 } else { index + 1 };
        if selected.contains(&order[index]) && !selected.contains(&order[neighbor]) {
            moves.push((order[index], order[neighbor]));
            order.swap(index, neighbor);
        }
    }
    moves
}
