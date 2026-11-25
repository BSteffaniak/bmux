//! Where the strip's tab list comes from.
//!
//! The tabs plugin publishes this attachment's projection of its personal tab
//! view (`bmux.tabs/personal-view`) to this attachment alone. When the bar
//! holds one, the strip shows that view's arrangement of the active workspace
//! in its order, with its names. Without one (the tabs plugin predates
//! personal views, or the attachment has no principal) the strip falls back
//! to the shared catalog (`tabs-list`) filtered to the selected tab's
//! workspace.
use bmux_tab_bar_plugin_api::status_items_list::{Severity, StatusItem};
use bmux_tabs_plugin_api::personal_view::{PersonalViewProjection, PersonalViewWorkspace};
use bmux_tabs_plugin_api::tabs_list::{TabListEntry, TabListSnapshot};
use uuid::Uuid;

/// The value before any projection arrives: the nil view means none.
pub const fn no_projection() -> PersonalViewProjection {
    PersonalViewProjection {
        last_removed: None,
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

/// How long the bar offers to undo a removal.
pub const UNDO_HINT_MS: u64 = 8_000;

fn hint(id: &str, text: String, severity: Severity, priority: i32) -> StatusItem {
    StatusItem {
        owner: super::OWNER.to_string(),
        id: id.to_string(),
        text,
        severity,
        priority,
        tooltip: None,
    }
}

/// The personal view's hints, shown before contributed status items: an
/// emptied view says how to add tabs, and a recent removal says how to undo
/// it (for [`UNDO_HINT_MS`] after the removal, by wall clock, so a replayed
/// projection never shows an old one again).
pub fn view_hints(
    projection: Option<&PersonalViewProjection>,
    no_tabs_shown: bool,
    now_ms: u64,
) -> Vec<StatusItem> {
    let Some(projection) = projection.filter(|projection| !projection.view_id.is_nil()) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    // The empty view's hint outranks the undo hint when space runs out.
    if no_tabs_shown {
        hints.push(hint(
            "personal-view-empty",
            "View empty: open the finder to add tabs".into(),
            Severity::Warning,
            i32::MAX,
        ));
    }
    if let Some(removal) = projection
        .last_removed
        .as_ref()
        .filter(|removal| now_ms < removal.removed_at_ms.saturating_add(UNDO_HINT_MS))
    {
        hints.push(hint(
            "personal-view-undo",
            format!("Removed {} (prefix u: undo)", removal.name),
            Severity::Info,
            i32::MAX - 1,
        ));
    }
    hints
}

/// When the undo hint of `projection` stops showing, if it still shows.
pub fn undo_hint_expiry_ms(
    projection: Option<&PersonalViewProjection>,
    now_ms: u64,
) -> Option<u64> {
    let expiry = projection?
        .last_removed
        .as_ref()?
        .removed_at_ms
        .saturating_add(UNDO_HINT_MS);
    (now_ms < expiry).then_some(expiry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tabs_plugin_api::personal_view::PersonalViewRemoval;

    fn projection(last_removed: Option<PersonalViewRemoval>) -> PersonalViewProjection {
        PersonalViewProjection {
            view_id: Uuid::from_u128(1),
            last_removed,
            ..no_projection()
        }
    }

    fn texts(hints: &[StatusItem]) -> Vec<&str> {
        hints.iter().map(|hint| hint.text.as_str()).collect()
    }

    #[test]
    fn hints_say_how_to_fill_an_empty_view_and_offer_undo_only_briefly() {
        // No projection (shared catalog), or a view with tabs: no hints.
        assert_eq!(view_hints(None, true, 0), Vec::new());
        assert_eq!(view_hints(Some(&projection(None)), false, 0), Vec::new());
        assert_eq!(
            texts(&view_hints(Some(&projection(None)), true, 0)),
            ["View empty: open the finder to add tabs"]
        );

        let removed = projection(Some(PersonalViewRemoval {
            context_id: Uuid::from_u128(7),
            name: "logs".into(),
            removed_at_ms: 10_000,
        }));
        assert_eq!(
            texts(&view_hints(Some(&removed), true, 10_000)),
            [
                "View empty: open the finder to add tabs",
                "Removed logs (prefix u: undo)"
            ]
        );
        assert_eq!(
            undo_hint_expiry_ms(Some(&removed), 10_000),
            Some(10_000 + UNDO_HINT_MS)
        );
        // Expired by wall clock, even when the projection is replayed.
        let later = 10_000 + UNDO_HINT_MS;
        assert_eq!(view_hints(Some(&removed), false, later), Vec::new());
        assert_eq!(undo_hint_expiry_ms(Some(&removed), later), None);
    }
}
