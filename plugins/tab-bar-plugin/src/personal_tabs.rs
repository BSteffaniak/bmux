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
        following: None,
        view_id: Uuid::nil(),
        name: String::new(),
        is_default: true,
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

/// The personal label this attachment's view gives `tab`, when it has one.
pub fn label_of(projection: Option<&PersonalViewProjection>, tab: Uuid) -> Option<&str> {
    projection?
        .workspaces
        .iter()
        .flat_map(|arrangement| &arrangement.members)
        .find(|member| member.context_id == tab)?
        .label
        .as_deref()
}

/// The first eight hex digits of a client id, enough to tell clients apart.
fn short_id(id: Uuid) -> String {
    id.simple().to_string().chars().take(8).collect()
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

/// The personal view's hints, shown before contributed status items: a
/// follow names the leader and whether the tab it shows is in this view, an
/// emptied view says how to add tabs, a recent removal says how to undo it
/// (for [`UNDO_HINT_MS`] after the removal, by wall clock, so a replayed
/// projection never shows an old one again), and a view other than the
/// default is named.
pub fn view_hints(
    projection: Option<&PersonalViewProjection>,
    no_tabs_shown: bool,
    now_ms: u64,
) -> Vec<StatusItem> {
    let Some(projection) = projection.filter(|projection| !projection.view_id.is_nil()) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    if let Some(follow) = &projection.following {
        hints.push(hint(
            "personal-view-following",
            if follow.in_view {
                format!(
                    "Following {} on {}",
                    short_id(follow.leader_client_id),
                    follow.name
                )
            } else {
                format!(
                    "Following {} on {} (not in your view)",
                    short_id(follow.leader_client_id),
                    follow.name
                )
            },
            Severity::Info,
            i32::MAX,
        ));
    }
    // The empty view's hint outranks the undo hint when space runs out. A
    // follow shows a tab even while the view has none here.
    if no_tabs_shown && projection.following.is_none() {
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
    if !projection.is_default {
        hints.push(hint(
            "personal-view-name",
            format!("View: {}", projection.name),
            Severity::Info,
            i32::MAX - 2,
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
    use bmux_tabs_plugin_api::personal_view::{PersonalViewFollow, PersonalViewRemoval};

    fn projection(last_removed: Option<PersonalViewRemoval>) -> PersonalViewProjection {
        PersonalViewProjection {
            view_id: Uuid::from_u128(1),
            name: "Default".into(),
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

    #[test]
    fn hints_name_a_view_other_than_the_default_and_a_follow() {
        let named = PersonalViewProjection {
            name: "review".into(),
            is_default: false,
            ..projection(None)
        };
        assert_eq!(texts(&view_hints(Some(&named), false, 0)), ["View: review"]);

        let leader = Uuid::parse_str("1234abcd-0000-0000-0000-000000000000").unwrap();
        let following = |in_view| PersonalViewProjection {
            following: Some(PersonalViewFollow {
                leader_client_id: leader,
                context_id: Uuid::from_u128(9),
                name: "logs".into(),
                in_view,
            }),
            ..projection(None)
        };
        // A followed tab outside the view is shown without the empty hint.
        assert_eq!(
            texts(&view_hints(Some(&following(false)), true, 0)),
            ["Following 1234abcd on logs (not in your view)"]
        );
        assert_eq!(
            texts(&view_hints(Some(&following(true)), false, 0)),
            ["Following 1234abcd on logs"]
        );
    }

    /// With a projection the strip is the view's arrangement: its order,
    /// its membership and its names, never the shared catalog's, even when
    /// the arrangement is empty. The catalog decides only without one.
    #[test]
    fn a_projection_alone_decides_the_strips_order_and_membership() {
        use bmux_tabs_plugin_api::personal_view::PersonalViewMember;
        let workspace = Uuid::from_u128(10);
        let entry = |id: u128, name: &str| TabListEntry {
            id: Uuid::from_u128(id),
            name: name.into(),
            active: false,
            workspace: "main".into(),
            workspace_id: workspace,
        };
        let catalog = TabListSnapshot {
            tabs: vec![entry(1, "one"), entry(2, "two"), entry(3, "three")],
            revision: 3,
        };
        let member = |id: u128, name: &str| PersonalViewMember {
            member_id: Uuid::from_u128(0x100 + id),
            context_id: Uuid::from_u128(id),
            name: name.into(),
            label: None,
            active: false,
        };
        let mut view = projection(None);
        view.active_workspace_id = workspace;
        view.workspaces = vec![PersonalViewWorkspace {
            workspace_id: workspace,
            members: vec![member(3, "three"), member(1, "mine")],
        }];
        let shown = |projected: &ProjectedTabs| {
            projected
                .tabs
                .tabs
                .iter()
                .map(|tab| (tab.id.as_u128(), tab.name.clone(), tab.active))
                .collect::<Vec<_>>()
        };
        let strip = from_projection(Some(&view), &catalog, Some(Uuid::from_u128(1)))
            .expect("a projection decides the strip");
        assert_eq!(
            shown(&strip),
            [(3, "three".into(), false), (1, "mine".into(), true)]
        );
        assert_eq!(strip.workspace_label.as_deref(), Some("main"));

        // An emptied arrangement stays empty; it never falls back.
        view.workspaces[0].members.clear();
        let empty = from_projection(Some(&view), &catalog, None).expect("still a projection");
        assert_eq!(shown(&empty), Vec::new());

        // No projection (or the nil view) leaves the catalog in charge.
        assert!(from_projection(None, &catalog, None).is_none());
        assert!(from_projection(Some(&no_projection()), &catalog, None).is_none());
        assert_eq!(
            shown(&from_catalog(&catalog, Some(Uuid::from_u128(2)))),
            [
                (1, "one".into(), false),
                (2, "two".into(), true),
                (3, "three".into(), false)
            ]
        );
    }

    #[test]
    fn labels_are_read_from_the_projection_members() {
        use bmux_tabs_plugin_api::personal_view::PersonalViewMember;
        let mut labelled = projection(None);
        labelled.workspaces = vec![PersonalViewWorkspace {
            workspace_id: Uuid::nil(),
            members: vec![
                PersonalViewMember {
                    member_id: Uuid::from_u128(1),
                    context_id: Uuid::from_u128(11),
                    name: "mine".into(),
                    label: Some("mine".into()),
                    active: true,
                },
                PersonalViewMember {
                    member_id: Uuid::from_u128(2),
                    context_id: Uuid::from_u128(12),
                    name: "shared".into(),
                    label: None,
                    active: false,
                },
            ],
        }];
        assert_eq!(label_of(Some(&labelled), Uuid::from_u128(11)), Some("mine"));
        assert_eq!(label_of(Some(&labelled), Uuid::from_u128(12)), None);
        assert_eq!(label_of(None, Uuid::from_u128(11)), None);
    }
}
