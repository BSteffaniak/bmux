//! Which tabs the sidebar lists, and in what order.
//!
//! The tabs plugin publishes this attachment's projection of its personal tab
//! view (`bmux.tabs/personal-view`) to this attachment alone, as it does for
//! the tab bar. When the sidebar holds one, every workspace the view arranges
//! lists that arrangement: its members in the view's order, with their
//! personal names. A workspace the view has not arranged yet lists its tabs in
//! the shared canonical order, which is exactly what the view will be seeded
//! with when this attachment first visits it. Without a projection (no
//! principal, or a tabs plugin without personal views) the shared catalog is
//! listed as it is.
use bmux_tabs_plugin_api::personal_view::PersonalViewProjection;
use bmux_tabs_plugin_api::tabs_list::{TabListEntry, TabListSnapshot};
use uuid::Uuid;

/// The tabs to list: `catalog` arranged by `personal` when it is present,
/// with `selected` (this attachment's own selection) marked active when
/// known.
pub fn arrange(
    catalog: &TabListSnapshot,
    personal: Option<&PersonalViewProjection>,
    selected: Option<Uuid>,
) -> TabListSnapshot {
    let mut snapshot = personal.map_or_else(|| catalog.clone(), |view| by_view(catalog, view));
    if let Some(selected) = selected {
        for tab in &mut snapshot.tabs {
            tab.active = tab.id == selected;
        }
    }
    snapshot
}

fn by_view(catalog: &TabListSnapshot, view: &PersonalViewProjection) -> TabListSnapshot {
    // Workspaces in the catalog's order, then any only the view knows.
    let mut workspaces: Vec<Uuid> = Vec::new();
    for workspace in catalog.tabs.iter().map(|tab| tab.workspace_id).chain(
        view.workspaces
            .iter()
            .map(|arrangement| arrangement.workspace_id),
    ) {
        if !workspaces.contains(&workspace) {
            workspaces.push(workspace);
        }
    }
    let label = |workspace: Uuid| {
        catalog
            .tabs
            .iter()
            .find(|tab| tab.workspace_id == workspace)
            .map(|tab| tab.workspace.clone())
            .unwrap_or_default()
    };
    let mut tabs = Vec::new();
    for workspace in workspaces {
        match view
            .workspaces
            .iter()
            .find(|arrangement| arrangement.workspace_id == workspace)
        {
            Some(arrangement) => {
                let name = label(workspace);
                tabs.extend(arrangement.members.iter().map(|member| TabListEntry {
                    id: member.context_id,
                    name: member.name.clone(),
                    active: member.active,
                    workspace: name.clone(),
                    workspace_id: workspace,
                }));
            }
            None => tabs.extend(
                catalog
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == workspace)
                    .cloned(),
            ),
        }
    }
    TabListSnapshot {
        tabs,
        // Either source changing must read as a change.
        revision: catalog.revision.wrapping_add(view.view_revision),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tabs_plugin_api::personal_view::{PersonalViewMember, PersonalViewWorkspace};

    fn entry(id: u128, name: &str, workspace: u128) -> TabListEntry {
        TabListEntry {
            id: Uuid::from_u128(id),
            name: name.into(),
            active: false,
            workspace: format!("ws{workspace}"),
            workspace_id: Uuid::from_u128(workspace),
        }
    }

    fn listed(snapshot: &TabListSnapshot) -> Vec<(String, String, bool)> {
        snapshot
            .tabs
            .iter()
            .map(|tab| (tab.workspace.clone(), tab.name.clone(), tab.active))
            .collect()
    }

    #[test]
    fn arranged_workspaces_follow_the_view_and_the_rest_preview_their_seed() {
        let catalog = TabListSnapshot {
            tabs: vec![
                entry(1, "one", 10),
                entry(2, "two", 10),
                entry(3, "three", 10),
                entry(4, "four", 20),
                entry(5, "five", 20),
            ],
            revision: 4,
        };
        let member = |id: u128, name: &str| PersonalViewMember {
            member_id: Uuid::from_u128(0xE00 + id),
            context_id: Uuid::from_u128(id),
            name: name.into(),
            active: false,
        };
        let view = PersonalViewProjection {
            view_id: Uuid::from_u128(0xF1),
            name: "default".into(),
            view_revision: 7,
            binding_revision: 1,
            active_workspace_id: Uuid::from_u128(10),
            // Workspace 10: three first, two left out, one relabelled.
            workspaces: vec![PersonalViewWorkspace {
                workspace_id: Uuid::from_u128(10),
                members: vec![member(3, "three"), member(1, "mine")],
            }],
        };
        let shown = arrange(&catalog, Some(&view), Some(Uuid::from_u128(1)));
        assert_eq!(
            listed(&shown),
            [
                ("ws10".into(), "three".into(), false),
                ("ws10".into(), "mine".into(), true),
                ("ws20".into(), "four".into(), false),
                ("ws20".into(), "five".into(), false),
            ]
        );
        // Without a view the catalog stands, with the local selection.
        let shared = arrange(&catalog, None, Some(Uuid::from_u128(2)));
        assert_eq!(shared.tabs.len(), 5);
        assert!(shared.tabs[1].active);
        // A newer view revision is a different listing even with the same
        // catalog.
        let mut newer = view;
        newer.view_revision = 8;
        assert_ne!(
            arrange(&catalog, Some(&newer), None).revision,
            shown.revision
        );
    }
}
