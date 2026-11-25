//! Sequential group effects with explicit partial-failure reporting.
//!
//! `left`, `right`, `close`, `move` and `new` act on shared tabs and the
//! shared order. `view-left`, `view-right` and `remove` act on this
//! attachment's personal view (the order the strip shows when it has one):
//! the tabs plugin resolves the view from the invoking attachment.
use super::{
    block_on_typed_dispatch, menu::MoveSelectionRequest, tabs_commands, tabs_personal_actions,
    workspaces_commands, workspaces_state,
};
use bmux_plugin::ServiceCallerDispatchClient;
use bmux_plugin_sdk::{ServiceResponse, prelude::NativeServiceContext};
use bmux_tabs_plugin_api::{tabs_personal_attachment, tabs_personal_views};

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
    if matches!(req.action.as_str(), "view-left" | "view-right" | "remove") {
        return personal(&req, &mut client);
    }
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
                error => errors.push(format!("{error:?} (tab {id})")),
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(fail(format!(
            "{}; completed {completed}; destination {destination}",
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

/// Group effects on the invoking attachment's personal view.
fn personal(
    req: &MoveSelectionRequest,
    client: &mut ServiceCallerDispatchClient<'_, NativeServiceContext>,
) -> Result<(), ServiceResponse> {
    let fail = |reason: String| ServiceResponse::error("group_action_failed", reason);
    let moves = if req.action == "remove" {
        Vec::new()
    } else {
        let binding =
            block_on_typed_dispatch(tabs_personal_attachment::client::current_binding(client))
                .map_err(|e| fail(e.to_string()))?
                .map_err(|e| fail(format!("{e:?}")))?;
        let view = block_on_typed_dispatch(tabs_personal_views::client::get_view(
            client,
            binding.view_id,
        ))
        .map_err(|e| fail(e.to_string()))?
        .map_err(|e| fail(format!("{e:?}")))?;
        let live = block_on_typed_dispatch(workspaces_state::client::get_workspace(
            client,
            workspaces_state::WorkspaceSelector {
                id: Some(req.workspace),
                name: None,
            },
        ))
        .map_err(|e| fail(e.to_string()))?
        .map_err(|e| fail(format!("{e:?}")))?
        .tab_ids;
        let order: Vec<uuid::Uuid> = view
            .workspaces
            .iter()
            .find(|arrangement| arrangement.workspace_id == req.workspace)
            .map(|arrangement| {
                arrangement
                    .members
                    .iter()
                    .map(|member| member.context_id)
                    .filter(|id| live.contains(id))
                    .collect()
            })
            .unwrap_or_default();
        if req.tabs.iter().any(|id| !order.contains(id)) {
            return Err(fail("Selection is no longer in this view".into()));
        }
        view_reorder_plan(order, &req.tabs, req.action == "view-left")
    };
    let mut completed = 0;
    let outcome = |result: Result<Result<_, _>, _>| match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("{error:?}")),
        Err(error) => Err(format!("{error}")),
    };
    if req.action == "remove" {
        let mut errors = Vec::new();
        for id in &req.tabs {
            match outcome(block_on_typed_dispatch(
                tabs_personal_actions::client::remove_tab(client, *id),
            )) {
                Ok(()) => completed += 1,
                Err(error) => errors.push(format!("{error} (tab {id})")),
            }
        }
        return if errors.is_empty() {
            Ok(())
        } else {
            Err(fail(format!(
                "{}; completed {completed}",
                errors.join("; ")
            )))
        };
    }
    for (source, before) in moves {
        if let Err(error) = outcome(block_on_typed_dispatch(
            tabs_personal_actions::client::move_tab(client, source, before),
        )) {
            return Err(fail(format!("{source}: {error}; completed {completed}")));
        }
        completed += 1;
    }
    Ok(())
}

/// The personal moves that shift every selected tab one place left (or
/// right) past its unselected neighbour, keeping the selection's relative
/// order: each is `(tab, the tab it then sits before, or none for the end)`.
fn view_reorder_plan(
    order: Vec<uuid::Uuid>,
    selected: &[uuid::Uuid],
    left: bool,
) -> Vec<(uuid::Uuid, Option<uuid::Uuid>)> {
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
            order.swap(index, neighbor);
            moves.push((order[neighbor], order.get(neighbor + 1).copied()));
        }
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::view_reorder_plan;
    use uuid::Uuid;

    fn apply(mut order: Vec<Uuid>, moves: &[(Uuid, Option<Uuid>)]) -> Vec<Uuid> {
        for (tab, before) in moves {
            order.retain(|id| id != tab);
            let at = before
                .and_then(|before| order.iter().position(|id| *id == before))
                .unwrap_or(order.len());
            order.insert(at, *tab);
        }
        order
    }

    #[test]
    fn view_moves_shift_the_selection_one_place_and_replay_in_order() {
        let [a, b, c, d] = [1, 2, 3, 4].map(Uuid::from_u128);
        let order = vec![a, b, c, d];
        let left = view_reorder_plan(order.clone(), &[b, d], true);
        assert_eq!(left, [(b, Some(a)), (d, Some(c))]);
        assert_eq!(apply(order.clone(), &left), [b, a, d, c]);
        let right = view_reorder_plan(order.clone(), &[a, c], false);
        assert_eq!(apply(order.clone(), &right), [b, a, d, c]);
        assert_eq!(right.last(), Some(&(a, Some(d))));
        // Already at the edge: nothing moves.
        assert_eq!(view_reorder_plan(order.clone(), &[a], true), []);
        assert_eq!(view_reorder_plan(order, &[c], false), [(c, None)]);
    }
}
