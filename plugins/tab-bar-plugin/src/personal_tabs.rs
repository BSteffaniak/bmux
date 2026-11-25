//! Where the strip's tab list comes from.
//!
//! The tabs plugin publishes this attachment's projection of its personal tab
//! view (`bmux.tabs/personal-view`) to this attachment alone. When the bar
//! holds one, the strip shows that view's arrangement of the active workspace
//! in its order, with its names. Without one (the tabs plugin predates
//! personal views, or the attachment has no principal) the strip falls back
//! to the shared catalog (`tabs-list`) filtered to the selected tab's
//! workspace.
use bmux_tabs_plugin_api::personal_view::{PersonalViewProjection, PersonalViewWorkspace};
use bmux_tabs_plugin_api::tabs_list::{TabListEntry, TabListSnapshot};
use uuid::Uuid;

/// The value before any projection arrives: the nil view means none.
pub const fn no_projection() -> PersonalViewProjection {
    PersonalViewProjection {
        view_id: Uuid::nil(),
        name: String::new(),
        view_revision: 0,
        binding_revision: 0,
        active_workspace_id: Uuid::nil(),
        workspaces: Vec::new(),
    }
}

/// The tabs the strip shows for one workspace.
pub struct ProjectedTabs {
    /// The workspace shown, when it is known to exist.
    pub workspace_id: Option<Uuid>,
    /// Its display name, when known.
    pub workspace_label: Option<String>,
    pub tabs: TabListSnapshot,
}

/// The strip's tabs from this attachment's personal view, or `None` when it
/// has no projection. `selected` is the attachment's local selection, which
/// can be newer than the projection's `active` flags.
pub fn from_projection(
    projection: Option<&PersonalViewProjection>,
    catalog: &TabListSnapshot,
    selected: Option<Uuid>,
) -> Option<ProjectedTabs> {
    let projection = projection.filter(|projection| !projection.view_id.is_nil())?;
    let contains = |arrangement: &&PersonalViewWorkspace, id: Uuid| {
        arrangement
            .members
            .iter()
            .any(|member| member.context_id == id)
    };
    // The arrangement holding the selected tab, else the workspace the
    // projection was made for.
    let arrangement = selected
        .and_then(|id| {
            projection
                .workspaces
                .iter()
                .find(|arrangement| contains(arrangement, id))
        })
        .or_else(|| {
            projection
                .workspaces
                .iter()
                .find(|arrangement| arrangement.workspace_id == projection.active_workspace_id)
        });
    let workspace_id = arrangement.map(|arrangement| arrangement.workspace_id);
    let workspace_label = workspace_id.and_then(|workspace| {
        catalog
            .tabs
            .iter()
            .find(|tab| tab.workspace_id == workspace)
            .map(|tab| tab.workspace.clone())
    });
    let tabs = arrangement
        .map(|arrangement| {
            arrangement
                .members
                .iter()
                .map(|member| TabListEntry {
                    id: member.context_id,
                    name: member.name.clone(),
                    active: selected.map_or(member.active, |id| id == member.context_id),
                    workspace: workspace_label.clone().unwrap_or_default(),
                    workspace_id: arrangement.workspace_id,
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ProjectedTabs {
        workspace_id,
        workspace_label,
        tabs: TabListSnapshot {
            tabs,
            revision: projection.view_revision,
        },
    })
}

/// The strip's tabs from the shared catalog: the selected tab's workspace
/// (the default workspace before any selection), marked by `selected`.
pub fn from_catalog(catalog: &TabListSnapshot, selected: Option<Uuid>) -> ProjectedTabs {
    let workspace = catalog
        .tabs
        .iter()
        .find(|tab| selected.map_or(tab.active, |id| tab.id == id))
        .map(|tab| tab.workspace_id)
        .or_else(|| selected.is_none().then_some(Uuid::nil()));
    let first = catalog
        .tabs
        .iter()
        .find(|tab| Some(tab.workspace_id) == workspace);
    let mut tabs = catalog.clone();
    tabs.tabs.retain(|tab| Some(tab.workspace_id) == workspace);
    if let Some(selected) = selected {
        for tab in &mut tabs.tabs {
            tab.active = tab.id == selected;
        }
    }
    ProjectedTabs {
        // A remembered label is not evidence of a live workspace identity.
        workspace_id: first.map(|tab| tab.workspace_id),
        workspace_label: first.map(|tab| tab.workspace.clone()),
        tabs,
    }
}
