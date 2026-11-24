#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/sidebar-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

mod activity;
mod view;

use bmux_plugin::layout::{
    LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest, PluginLayoutSnapshot,
};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceSnapshot,
};
use bmux_plugin::{
    AttachInputEvent, AttachInputResult, ExtensionRect, RenderColor, RenderNamedColor, RenderStyle,
    ServiceCallerDispatchClient, block_on_typed_dispatch,
};
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::{TypedServiceEndpoint as _, prelude::*};
use bmux_presentation_scene::scene::{HitTable, HitTables};
use bmux_presentation_state::visibility::{
    PresentationVisibility, VisibilityCommand, VisibilitySettings,
};
use bmux_presentation_state::{PresentationEntityRef, PresentationFact, PresentationFactRole};
use bmux_tabs_plugin_api::{tabs_list, tabs_status};
use bmux_tui_components::virtual_list::{CursorInput, CursorOutcome, ItemCursor};
use std::sync::Mutex;
use uuid::Uuid;

const OWNER: &str = "bmux.sidebar";
const LAYOUT_ID: &str = "sidebar";
const SURFACE_ID: &str = "sidebar";
const RETAINED_ID: Uuid = Uuid::from_u128(0x626d_7578_5f73_6964_6562_6172_0000_0001);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Left,
    Right,
}

/// How the sidebar organizes its tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grouping {
    /// One flat list.
    None,
    /// Under a collapsible header per workspace.
    Workspace,
}

/// Which tabs the sidebar lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    AllWorkspaces,
    /// Only the workspace of the tab this client shows.
    CurrentWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    placement: Placement,
    grouping: Grouping,
    scope: Scope,
    width: u16,
    minimum_width: u16,
    maximum_width: u16,
    order: i32,
    show_index: bool,
    heading: String,
    title_template: String,
    description_template: String,
    status_template: String,
    maximum_visible_items: usize,
    content_height: bool,
    collapse_below_width: u16,
    collapsed_width: u16,
    visibility: VisibilitySettings,
    remember: Remember,
}

/// What the sidebar remembers on this machine between starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Remember {
    /// The width last dragged or stepped to.
    width: bool,
    /// Which groups were collapsed.
    groups: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            placement: Placement::Left,
            width: 28,
            minimum_width: 16,
            maximum_width: 60,
            order: 200,
            show_index: true,
            heading: "Tabs".to_string(),
            title_template: "{state_icon} {name}".to_string(),
            description_template: "{detail}".to_string(),
            status_template: String::new(),
            maximum_visible_items: 20,
            content_height: false,
            collapse_below_width: 80,
            collapsed_width: 8,
            // Enabled by default but closed until a client opens it; the tab
            // bar is the default visible tab presentation.
            visibility: VisibilitySettings {
                visible: false,
                ..VisibilitySettings::default()
            },
            remember: Remember {
                width: true,
                groups: true,
            },
            grouping: Grouping::None,
            scope: Scope::AllWorkspaces,
        }
    }
}

impl Settings {
    fn parse(value: Option<&toml::Value>) -> Result<Self, PluginCommandError> {
        let mut settings = Self::default();
        let Some(table) = value.and_then(toml::Value::as_table) else {
            return Ok(settings);
        };
        if let Some(placement) = table.get("placement").and_then(toml::Value::as_str) {
            settings.placement = match placement {
                "left" => Placement::Left,
                "right" => Placement::Right,
                other => {
                    return Err(PluginCommandError::invalid_arguments(format!(
                        "bmux.sidebar placement must be 'left' or 'right', got {other:?}"
                    )));
                }
            };
        }
        parse_organization(table, &mut settings)?;
        for (key, target) in [
            ("width", &mut settings.width),
            ("minimum_width", &mut settings.minimum_width),
            ("maximum_width", &mut settings.maximum_width),
            ("collapse_below_width", &mut settings.collapse_below_width),
            ("collapsed_width", &mut settings.collapsed_width),
        ] {
            if let Some(value) = table.get(key).and_then(toml::Value::as_integer) {
                *target = u16::try_from(value)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        PluginCommandError::invalid_arguments(format!(
                            "bmux.sidebar {key} must be a positive cell count"
                        ))
                    })?;
            }
        }
        if settings.minimum_width > settings.width || settings.width > settings.maximum_width {
            return Err(PluginCommandError::invalid_arguments(
                "bmux.sidebar requires minimum_width <= width <= maximum_width",
            ));
        }
        if settings.collapsed_width > settings.width {
            return Err(PluginCommandError::invalid_arguments(
                "bmux.sidebar collapsed_width must not exceed width",
            ));
        }
        if let Some(order) = table.get("order").and_then(toml::Value::as_integer) {
            settings.order = i32::try_from(order).map_err(|_| {
                PluginCommandError::invalid_arguments("bmux.sidebar order must fit in an i32")
            })?;
        }
        if let Some(show_index) = table.get("show_index").and_then(toml::Value::as_bool) {
            settings.show_index = show_index;
        }
        for (key, target) in [
            ("heading", &mut settings.heading),
            ("title_template", &mut settings.title_template),
            ("description_template", &mut settings.description_template),
            ("status_template", &mut settings.status_template),
        ] {
            if let Some(value) = table.get(key).and_then(toml::Value::as_str) {
                if value.len() > 4_096 {
                    return Err(PluginCommandError::invalid_arguments(format!(
                        "bmux.sidebar {key} must not exceed 4096 bytes"
                    )));
                }
                *target = value.to_string();
            }
        }
        if let Some(content_height) = table.get("content_height").and_then(toml::Value::as_bool) {
            settings.content_height = content_height;
        }
        for (key, target) in [
            ("visible", &mut settings.visibility.visible),
            ("remember_visibility", &mut settings.visibility.remember),
            ("remember_width", &mut settings.remember.width),
            ("remember_groups", &mut settings.remember.groups),
        ] {
            if let Some(value) = table.get(key) {
                *target = value.as_bool().ok_or_else(|| {
                    PluginCommandError::invalid_arguments(format!(
                        "bmux.sidebar {key} must be true or false"
                    ))
                })?;
            }
        }
        if let Some(count) = table
            .get("maximum_visible_items")
            .and_then(toml::Value::as_integer)
        {
            settings.maximum_visible_items = usize::try_from(count)
                .ok()
                .filter(|count| *count > 0 && *count <= 1_024)
                .ok_or_else(|| {
                    PluginCommandError::invalid_arguments(
                        "bmux.sidebar maximum_visible_items must be between 1 and 1024",
                    )
                })?;
        }
        Ok(settings)
    }
}

/// `grouping` and `scope`: how the sidebar organizes its tabs and which it lists.
fn parse_organization(
    table: &toml::map::Map<String, toml::Value>,
    settings: &mut Settings,
) -> Result<(), PluginCommandError> {
    if let Some(grouping) = table.get("grouping") {
        settings.grouping = match grouping.as_str() {
            Some("none") => Grouping::None,
            Some("workspace") => Grouping::Workspace,
            _ => {
                return Err(PluginCommandError::invalid_arguments(format!(
                    "bmux.sidebar grouping must be 'none' or 'workspace', got {grouping}"
                )));
            }
        };
    }
    if let Some(scope) = table.get("scope") {
        settings.scope = match scope.as_str() {
            Some("all_workspaces") => Scope::AllWorkspaces,
            Some("current_workspace") => Scope::CurrentWorkspace,
            _ => {
                return Err(PluginCommandError::invalid_arguments(format!(
                    "bmux.sidebar scope must be 'all_workspaces' or 'current_workspace', got {scope}"
                )));
            }
        };
    }
    Ok(())
}

/// Where a presentation reads semantic facts from.
enum FactSource {
    /// A registry shared explicitly with this runtime's producers.
    Registry(std::sync::Arc<bmux_presentation_state::PresentationFactRegistry>),
    /// The fact host in this runtime's shared state registry, where other
    /// companions of the same runtime publish facts.
    Host(std::sync::Arc<std::sync::RwLock<bmux_presentation_state::PresentationFactHostService>>),
}

impl FactSource {
    fn with<R>(
        &self,
        read: impl FnOnce(&bmux_presentation_state::PresentationFactRegistry) -> R,
    ) -> Option<R> {
        match self {
            Self::Registry(registry) => Some(read(registry)),
            Self::Host(host) => host.read().ok().map(|host| read(host.registry())),
        }
    }

    fn subscribe(
        &self,
    ) -> Result<tokio::sync::watch::Receiver<bmux_presentation_state::PresentationFactChange>, String>
    {
        self.with(bmux_presentation_state::PresentationFactRegistry::subscribe_changes)
            .ok_or_else(|| "presentation facts unavailable".to_string())
    }
}

impl std::fmt::Debug for FactSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Registry(_) => "FactSource::Registry",
            Self::Host(_) => "FactSource::Host",
        })
    }
}

#[derive(Debug)]
struct CompanionState {
    // None selects the legacy process-local presentation until runtime ownership
    // is threaded through installation; explicit owners never publish globally.
    surfaces: Option<std::sync::Arc<bmux_plugin::surface::PluginSurfaceRegistry>>,
    layouts: Option<std::sync::Arc<bmux_plugin::layout::PluginLayoutRegistry>>,
    facts: Option<FactSource>,
    visibility: PresentationVisibility,
    subscription: Option<tokio::task::JoinHandle<()>>,
    allocation: Option<ExtensionRect>,
    allocation_publication_pending: bool,
    presentation_pending: std::cell::Cell<bool>,
    settings: Settings,
    revision: u64,
    /// The broadcast catalog as received, before client-local selection.
    catalog: tabs_list::TabListSnapshot,
    /// This attachment's own selection (`tabs-local-view`), when known.
    selected_context_id: Option<Uuid>,
    snapshot: tabs_list::TabListSnapshot,
    /// The highlighted row, moved by the pointer (hover) and the keyboard.
    cursor: ItemCursor<SidebarHit>,
    /// Workspaces whose tabs are hidden under their header.
    collapsed: std::collections::BTreeSet<Uuid>,
    /// Where collapsed groups are remembered, when they are.
    groups_memory: Option<std::path::PathBuf>,
    /// What each tab is doing, from the tabs plugin's `tabs-status`.
    activity: activity::ActivityView,
    /// The rows' measurements and scroll position.
    list: std::cell::RefCell<bmux_tui_components::virtual_list::VirtualListState<SidebarHit>>,
    /// What each published revision's region keys mean.
    hits: std::cell::RefCell<HitTables<SidebarHit>>,
    /// The width the sidebar asks for, within its minimum and maximum.
    width: u16,
    /// Where the width is remembered, when it is.
    width_memory: Option<std::path::PathBuf>,
    /// A drag of the resize edge in progress.
    resize: Option<ResizeDrag>,
    /// How the pointer and keyboard are engaged with the sidebar.
    engaged: Engagement,
    /// The colors of the column facing the panes, from the appearance.
    edge_colors: EdgeColors,
}

/// How the pointer and keyboard are engaged with the sidebar's chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Engagement {
    /// The pointer is over the resize edge.
    edge_hovered: bool,
    /// The sidebar has keyboard focus in this client.
    focused: bool,
}

/// A drag of the sidebar's resize edge: where the pointer pressed, on the
/// screen, and the width then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeDrag {
    start_col: u16,
    start_width: u16,
    /// Where on the scrollbar's track it was pressed, and how far below the
    /// thumb's top when it was pressed on the thumb.
    start_row: i32,
    grab: Option<i32>,
    mode: ColumnGesture,
}

/// What a press on the column facing the panes turned out to be, decided by
/// its first movement: sideways resizes, up or down from the thumb scrolls,
/// and none at all is a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnGesture {
    Undecided,
    Resize,
    Scroll,
}

/// The scrollbar thumb's pointer: a hand that grabs it, closed while it is
/// dragged to scroll, and the resize arrows once a press on it became a resize.
const fn thumb_cursor(drag: Option<ColumnGesture>) -> bmux_plugin::surface::PluginSurfaceCursor {
    use bmux_plugin::surface::PluginSurfaceCursor;
    use bmux_tui::hit::HitRole;
    use bmux_tui::pointer::{PointerIntent, PointerShape};
    let shape = match drag {
        Some(ColumnGesture::Resize) => return PluginSurfaceCursor::ResizeHorizontal,
        Some(ColumnGesture::Scroll) => PointerShape::Grabbing,
        Some(ColumnGesture::Undecided) | None => PointerShape::Grab,
    };
    PluginSurfaceCursor::Tui {
        intent: PointerIntent::Shape(shape),
        role: HitRole::DragHandle,
    }
}

/// Cells `widen` and `narrow` move the edge by.
const WIDTH_STEP: u16 = 2;
/// The file in the local state directory that remembers the width.
const WIDTH_FILE: &str = "sidebar-width";

/// The typed meaning of a sidebar region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SidebarHit {
    Tab(Uuid),
    /// A workspace's header, by workspace id.
    Group(Uuid),
    /// The edge facing the panes, dragged to resize the sidebar.
    ResizeEdge,
    /// The list's scrollbar.
    Scrollbar,
}

/// One row of the sidebar's list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SidebarRow {
    /// A workspace's header: the indices of its listed tabs in the snapshot,
    /// and whether they are hidden under it.
    Group {
        id: Uuid,
        name: String,
        tabs: Vec<usize>,
        collapsed: bool,
    },
    /// A tab, by its index in the snapshot.
    Tab(usize),
}

impl SidebarRow {
    /// The row's identity, for the cursor, hit regions and measurements.
    const fn item(&self, tabs: &[tabs_list::TabListEntry]) -> SidebarHit {
        match self {
            Self::Group { id, .. } => SidebarHit::Group(*id),
            Self::Tab(index) => SidebarHit::Tab(tabs[*index].id),
        }
    }
}

impl std::fmt::Display for SidebarHit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&item_region_id(*self))
    }
}

/// The owner-local region id of a row, and its inverse.
fn item_region_id(item: SidebarHit) -> String {
    match item {
        SidebarHit::Tab(id) => tab_region_id(id),
        SidebarHit::Group(id) => format!("group:{id}"),
        SidebarHit::ResizeEdge => RESIZE_EDGE_REGION.to_string(),
        SidebarHit::Scrollbar => view::SCROLLBAR.to_string(),
    }
}

fn region_item(region: &str) -> Option<SidebarHit> {
    if region == RESIZE_EDGE_REGION {
        return Some(SidebarHit::ResizeEdge);
    }
    if region == view::SCROLLBAR {
        return Some(SidebarHit::Scrollbar);
    }
    if let Some(id) = region.strip_prefix("group:") {
        return Uuid::parse_str(id).ok().map(SidebarHit::Group);
    }
    region_tab_id(region).map(SidebarHit::Tab)
}

/// The file in the local state directory that remembers collapsed groups.
const GROUPS_FILE: &str = "sidebar-collapsed-groups";

/// The owner-local region id of a tab row, and its inverse, for events without
/// a typed key (the stateless service fallback has no hit table).
fn tab_region_id(id: Uuid) -> String {
    format!("tab:{id}")
}

fn region_tab_id(region: &str) -> Option<Uuid> {
    region
        .strip_prefix("tab:")
        .and_then(|id| Uuid::parse_str(id).ok())
}

impl Drop for CompanionState {
    fn drop(&mut self) {
        if let Some(task) = self.subscription.take() {
            task.abort();
        }
        if let Some(registry) = &self.surfaces {
            registry.remove_owner(OWNER);
        }
    }
}

impl CompanionState {
    fn new(settings: Settings, local_state_dir: Option<std::path::PathBuf>) -> Self {
        let activity = activity::ActivityView::default();
        let width_memory = local_state_dir
            .as_deref()
            .filter(|_| settings.remember.width)
            .map(|directory| directory.join(WIDTH_FILE));
        let groups_memory = local_state_dir
            .as_deref()
            .filter(|_| settings.remember.groups)
            .map(|directory| directory.join(GROUPS_FILE));
        let collapsed = groups_memory
            .as_deref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| Uuid::parse_str(line.trim()).ok())
                    .collect()
            })
            .unwrap_or_default();
        let width = width_memory
            .as_deref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| text.trim().parse::<u16>().ok())
            .map_or(settings.width, |width| {
                width.clamp(settings.minimum_width, settings.maximum_width)
            });
        Self {
            collapsed,
            groups_memory,
            width,
            width_memory,
            resize: None,
            engaged: Engagement::default(),
            edge_colors: EdgeColors::default(),
            surfaces: None,
            layouts: None,
            facts: None,
            visibility: PresentationVisibility::new(settings.visibility, local_state_dir),
            subscription: None,
            allocation: None,
            allocation_publication_pending: false,
            presentation_pending: std::cell::Cell::new(false),
            hits: std::cell::RefCell::new(HitTables::new(4)),
            settings,
            revision: 0,
            catalog: tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 0,
            },
            selected_context_id: None,
            snapshot: tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 0,
            },
            cursor: ItemCursor::new(),
            activity,
            list: std::cell::RefCell::new(
                bmux_tui_components::virtual_list::VirtualListState::new(0),
            ),
        }
    }

    const fn visible(&self) -> bool {
        self.visibility.visible()
    }

    const fn allocated_width(&self) -> u16 {
        match self.allocation {
            Some(rect) => rect.w,
            None => self.width,
        }
    }

    /// Take a focus change for the region `hook` names: the focused row is
    /// the keyboard's, and is highlighted. Returns whether anything changed
    /// (and republishes it).
    fn set_focus(&mut self, hook: &str, focused: bool) -> bool {
        // `owner:surface:region`, where the region id has colons of its own.
        let row = hook
            .splitn(3, ':')
            .nth(2)
            .and_then(region_item)
            .filter(|item| matches!(item, SidebarHit::Tab(_) | SidebarHit::Group(_)));
        let previous = (self.engaged.focused, self.cursor.clone());
        self.engaged.focused = focused;
        if focused {
            if let Some(row) = row {
                self.cursor.set_cursor(Some(row));
                self.cursor.set_hovered(Some(row));
            }
        } else {
            self.cursor.set_hovered(None);
        }
        if previous == (self.engaged.focused, self.cursor.clone()) {
            return false;
        }
        self.revision = self.revision.saturating_add(1).max(1);
        publish_companion(self).is_ok()
    }

    /// Ask for `width`, kept within the minimum and maximum. Returns whether
    /// the width changed.
    fn set_width(&mut self, width: u16) -> bool {
        let width = width.clamp(self.settings.minimum_width, self.settings.maximum_width);
        if width == self.width {
            return false;
        }
        self.width = width;
        true
    }

    /// Remember the width for the next start, when this installation does.
    ///
    /// # Errors
    /// Returns a description of a failure to write the remembered width.
    fn remember_width(&self) -> Result<(), String> {
        let Some(file) = &self.width_memory else {
            return Ok(());
        };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Replace the file whole so a concurrent reader never sees a partial write.
            let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&temporary, self.width.to_string())?;
            std::fs::rename(&temporary, file)
        };
        write().map_err(|error| format!("remembering sidebar width in {}: {error}", file.display()))
    }

    fn resize(&mut self, rect: ExtensionRect) -> bool {
        if self.allocation == Some(rect) {
            return false;
        }
        self.allocation = Some(rect);
        self.settle_scroll(true);
        true
    }

    fn publish_allocation(
        &mut self,
        rect: ExtensionRect,
        publish: impl FnOnce(&Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let changed = self.resize(rect);
        if !changed && !self.allocation_publication_pending {
            return Ok(());
        }
        self.allocation_publication_pending = true;
        self.presentation_pending.set(true);
        publish(self)?;
        self.presentation_pending.set(false);
        self.allocation_publication_pending = false;
        Ok(())
    }

    fn tab_fact(&self, tab: &tabs_list::TabListEntry) -> Option<PresentationFact> {
        self.facts
            .as_ref()
            .and_then(|facts| facts.with(|registry| tab_fact(tab, registry)))
            .flatten()
    }

    /// Every listed row as a component, keyed by what it shows.
    fn row_views(&self, revision: u64) -> Vec<(SidebarHit, view::RowView)> {
        let palette = SidebarPalette::default();
        self.rows()
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let item = row.item(&self.snapshot.tabs);
                let hovered = self.cursor.hovered() == Some(&item);
                let row = match row {
                    SidebarRow::Tab(tab) => {
                        view::RowView::Card(self.card(*tab, index, hovered, revision, &palette))
                    }
                    SidebarRow::Group {
                        name,
                        tabs,
                        collapsed,
                        ..
                    } => view::RowView::Header(
                        self.header(name, tabs, *collapsed, hovered, &palette),
                    ),
                };
                (item, row)
            })
            .collect()
    }

    /// A tab's card: its title (the state glyph in its state's color when the
    /// title starts with it) with the state summary at the right, its
    /// description and status, tinted when shown or hovered.
    fn card(
        &self,
        tab_index: usize,
        row: usize,
        hovered: bool,
        revision: u64,
        palette: &SidebarPalette,
    ) -> view::CardRow {
        use bmux_plugin::render_paint::tui_style;
        let tab = &self.snapshot.tabs[tab_index];
        let fact = self.tab_fact(tab);
        let width = usize::from(self.allocated_width().saturating_sub(4));
        let activity = self
            .activity
            .activity(tab.id, tab.active)
            .map(|activity| activity.fitted(width.saturating_sub(2)));
        let render = |template: &str| {
            render_template(
                template,
                tab,
                tab_index,
                self.settings.show_index,
                fact.as_ref(),
                activity.as_ref(),
            )
        };
        let card = if tab.active {
            Some(palette.active_card)
        } else if hovered {
            Some(palette.hover_card)
        } else {
            None
        };
        let title_style = fact.as_ref().map_or_else(
            || {
                if tab.active {
                    palette.active_title
                } else {
                    palette.title
                }
            },
            |fact| fact_style(fact.role, palette.title),
        );
        let title = render(&self.settings.title_template);
        let (glyph, color) = state_glyph(activity.as_ref());
        let mut spans = Vec::new();
        let rest = match (&activity, title.strip_prefix(glyph)) {
            (Some(_), Some(rest)) => {
                spans.push(bmux_tui::text::Span::styled(
                    glyph.to_string(),
                    tui_style(RenderStyle::new().foreground(color)),
                ));
                rest.to_string()
            }
            _ => title,
        };
        spans.push(bmux_tui::text::Span::styled(rest, tui_style(title_style)));
        let trailing = activity.as_ref().and_then(|activity| {
            let style = match activity.state {
                activity::TabState::Idle => palette.muted,
                state => RenderStyle::new().foreground(state.color()),
            };
            activity
                .summary
                .clone()
                .map(|summary| view::styled(summary, tui_style(style)))
        });
        let field = |name: &str| view::FieldId {
            id: selection_id(row, name),
            revision,
        };
        view::CardRow {
            title: bmux_tui::text::Line::from_spans(spans),
            trailing,
            description: render(&self.settings.description_template),
            status: render(&self.settings.status_template),
            muted: tui_style(palette.muted),
            chrome: view::RowChrome {
                card: card.map(tui_style),
                accent: tab.active.then(|| tui_style(palette.accent)),
                base: tui_style(palette.base),
            },
            fields: [field("title"), field("description"), field("status")],
        }
    }

    /// A workspace's header: its marker and name, and at the right what its
    /// tabs are doing, most pressing first, or how many it holds. A collapsed
    /// header that hides the shown tab is marked as the shown row.
    fn header(
        &self,
        name: &str,
        tabs: &[usize],
        collapsed: bool,
        hovered: bool,
        palette: &SidebarPalette,
    ) -> view::HeaderRow {
        use bmux_plugin::render_paint::{tui_color, tui_style};
        let holds_active = collapsed && tabs.iter().any(|index| self.snapshot.tabs[*index].active);
        let card = if holds_active {
            Some(palette.active_card)
        } else if hovered {
            Some(palette.hover_card)
        } else {
            None
        };
        let marker = if collapsed { '▸' } else { '▾' };
        let title = format!("{marker} {name}");
        // The title keeps at least a few cells; the counts take the rest, the
        // least pressing dropped first when they do not fit.
        let title_room = bmux_cell_geometry::display_width(&title).min(8);
        let room =
            usize::from(self.allocated_width().saturating_sub(4)).saturating_sub(title_room + 1);
        let mut spans = Vec::new();
        for (index, (text, color)) in group_summary(self, tabs, room).into_iter().enumerate() {
            if index > 0 {
                spans.push(bmux_tui::text::Span::raw(" "));
            }
            let style = color.map_or_else(
                || tui_style(palette.muted),
                |color| bmux_tui::style::Style::new().fg(tui_color(color)),
            );
            spans.push(bmux_tui::text::Span::styled(text, style));
        }
        view::HeaderRow {
            title: view::styled(title, tui_style(palette.heading)),
            trailing: bmux_tui::text::Line::from_spans(spans),
            chrome: view::RowChrome {
                card: card.map(tui_style),
                accent: holds_active.then(|| tui_style(palette.accent)),
                base: tui_style(palette.base),
            },
        }
    }

    /// The rows as a list, measured at the current width. Measurements are
    /// kept per row and content, so this is cheap when little changed.
    fn synced_list<'a>(
        &self,
        rows: &'a [(SidebarHit, view::RowView)],
    ) -> bmux_tui_components::virtual_list::VirtualList<'a, SidebarHit> {
        let list = view::list(rows);
        let keys = rows.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        let ((_, width), _) = self.list_columns(self.list_height(&keys));
        list.sync(
            u64::from(width),
            &mut self.list.borrow_mut(),
            &mut bmux_tui::component::LayoutCx::new(),
        );
        list
    }

    /// The list's first column and width, and the scrollbar's column: the
    /// whole width while the rows fit; while they overflow, the scrollbar
    /// shares the resize edge's column and the list takes the rest.
    fn list_columns(&self, list_height: u16) -> ((u16, u16), Option<u16>) {
        let width = self.allocated_width();
        let overflows = self.list.borrow().total_height() > u64::from(list_height);
        if !overflows || width < 4 {
            return ((0, width), None);
        }
        match self.settings.placement {
            Placement::Left => ((0, width - 1), Some(width - 1)),
            Placement::Right => ((1, width - 1), Some(0)),
        }
    }

    /// The scrollbar's track height and layout, while the rows overflow.
    fn scrollbar_geometry(&self) -> Option<(u16, bmux_tui_components::scrollbar::ScrollbarLayout)> {
        let keys = self.item_keys();
        let height = self.list_height(&keys);
        self.list_columns(height).1?;
        let state = self.list.borrow().scrollbar_state(u64::from(height));
        let layout = bmux_tui_components::scrollbar::Scrollbar::new()
            .layout(bmux_tui::geometry::Rect::new(0, 0, 1, height), &state);
        Some((height, layout))
    }

    /// Scroll so the thumb starts at track row `top`, by the list's own
    /// scrollbar action.
    fn scroll_thumb_to(&self, layout: bmux_tui_components::scrollbar::ScrollbarLayout, top: i32) {
        let travel = i32::from(layout.track_len.saturating_sub(layout.thumb_len));
        if travel == 0 {
            return;
        }
        let fraction = f64::from(top.clamp(0, travel)) / f64::from(travel);
        self.list.borrow_mut().apply_action(
            &bmux_tui_components::virtual_list::VirtualListAction::ScrollbarDrag(fraction),
        );
    }

    /// The pointer's row on the scrollbar's track, which starts below the
    /// heading.
    fn track_row(&self, event: &AttachInputEvent) -> Option<i32> {
        let top = i32::from(self.allocation.map_or(0, |rect| rect.y)) + 1;
        event
            .screen_row
            .map(|row| i32::from(row) - top)
            .or_else(|| event.row.map(|row| i32::from(row) - 1))
    }

    /// Rows the list may use below the heading: the allocation's, or the
    /// first `maximum_visible_items` rows' when nothing is allocated yet.
    fn list_height(&self, keys: &[SidebarHit]) -> u16 {
        self.allocation.map_or_else(
            || {
                let state = self.list.borrow();
                let rows = keys
                    .iter()
                    .take(self.settings.maximum_visible_items)
                    .filter_map(|key| state.item_height(key))
                    .sum::<u64>();
                u16::try_from(rows).unwrap_or(u16::MAX)
            },
            |rect| rect.h.saturating_sub(2),
        )
    }

    /// Keep the scroll position within the rows and, when `reveal_active`,
    /// on the row showing this client's tab.
    fn settle_scroll(&self, reveal_active: bool) {
        let rows = self.row_views(self.revision);
        let keys = rows.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        let _list = self.synced_list(&rows);
        let height = u64::from(self.list_height(&keys));
        let active = reveal_active.then(|| self.active_item()).flatten();
        let mut state = self.list.borrow_mut();
        state.clamp_scroll(height);
        if let Some(key) = active {
            state.ensure_key_visible(&key, height);
        }
    }

    /// Scroll so `item`'s row is visible.
    fn reveal_item(&self, item: SidebarHit) {
        let rows = self.row_views(self.revision);
        let keys = rows.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        let _list = self.synced_list(&rows);
        let height = u64::from(self.list_height(&keys));
        self.list.borrow_mut().ensure_key_visible(&item, height);
    }

    /// Scroll by `rows`; returns whether the position moved.
    fn scroll_rows(&self, rows: i64) -> bool {
        let views = self.row_views(self.revision);
        let keys = views.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        let _list = self.synced_list(&views);
        let height = u64::from(self.list_height(&keys));
        self.list.borrow_mut().scroll_by(rows, height)
    }

    /// The first listed row's offset into the list.
    #[cfg(test)]
    fn scroll_offset(&self) -> u64 {
        self.list.borrow().scroll.vertical_offset()
    }

    /// Tabs in scope, including those under collapsed headers.
    fn tabs_in_scope(&self) -> usize {
        self.rows()
            .iter()
            .map(|row| match row {
                SidebarRow::Tab(_) => 1,
                SidebarRow::Group {
                    tabs, collapsed, ..
                } => {
                    if *collapsed {
                        tabs.len()
                    } else {
                        0
                    }
                }
            })
            .sum()
    }

    /// The row showing this client's tab.
    fn active_item(&self) -> Option<SidebarHit> {
        self.active_row().map(|row| self.item_keys()[row])
    }

    /// The listed rows: every tab in scope, under its workspace's header when
    /// grouped, with a collapsed workspace's tabs left out.
    fn rows(&self) -> Vec<SidebarRow> {
        let tabs = &self.snapshot.tabs;
        let current = tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.workspace_id);
        let listed = tabs.iter().enumerate().filter(|(_, tab)| {
            self.settings.scope == Scope::AllWorkspaces
                || current.is_none_or(|current| tab.workspace_id == current)
        });
        if self.settings.grouping == Grouping::None {
            return listed.map(|(index, _)| SidebarRow::Tab(index)).collect();
        }
        // The catalog lists tabs in workspace order; keep a workspace's tabs
        // together under the first place it appears.
        let mut groups: Vec<(Uuid, String, Vec<usize>)> = Vec::new();
        for (index, tab) in listed {
            match groups.iter_mut().find(|(id, ..)| *id == tab.workspace_id) {
                Some((.., members)) => members.push(index),
                None => groups.push((tab.workspace_id, tab.workspace.clone(), vec![index])),
            }
        }
        let mut rows = Vec::new();
        for (id, name, members) in groups {
            let collapsed = self.collapsed.contains(&id);
            rows.push(SidebarRow::Group {
                id,
                name,
                tabs: members.clone(),
                collapsed,
            });
            if !collapsed {
                rows.extend(members.into_iter().map(SidebarRow::Tab));
            }
        }
        rows
    }

    /// The row showing this client's tab: its own, or its collapsed group's.
    fn active_row(&self) -> Option<usize> {
        let active = self.snapshot.tabs.iter().position(|tab| tab.active)?;
        self.rows().iter().position(|row| match row {
            SidebarRow::Tab(index) => *index == active,
            SidebarRow::Group {
                tabs, collapsed, ..
            } => *collapsed && tabs.contains(&active),
        })
    }

    /// Every listed row's identity, in order.
    fn item_keys(&self) -> Vec<SidebarHit> {
        self.rows()
            .iter()
            .map(|row| row.item(&self.snapshot.tabs))
            .collect()
    }

    /// Collapse or expand `workspace`'s group and remember it. Returns a
    /// description of a failure to remember it.
    fn toggle_group(&mut self, workspace: Uuid) -> Option<String> {
        if !self.collapsed.remove(&workspace) {
            self.collapsed.insert(workspace);
        }
        self.revision = self.revision.saturating_add(1).max(1);
        self.settle_scroll(false);
        let keys = self.item_keys();
        self.cursor.retain(&keys);
        self.remember_groups().err()
    }

    /// Remember the collapsed groups for the next start, when this
    /// installation does.
    ///
    /// # Errors
    /// Returns a description of a failure to write them.
    fn remember_groups(&self) -> Result<(), String> {
        let Some(file) = &self.groups_memory else {
            return Ok(());
        };
        let text = self
            .collapsed
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let write = || -> std::io::Result<()> {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
            std::fs::write(&temporary, text)?;
            std::fs::rename(&temporary, file)
        };
        write().map_err(|error| {
            format!(
                "remembering collapsed sidebar groups in {}: {error}",
                file.display()
            )
        })
    }

    /// Apply a catalog snapshot. The shared catalog cannot know which tab is
    /// this attachment's, so a known client-local selection decides `active`,
    /// exactly as the tab bar does.
    fn replace_tabs(&mut self, mut snapshot: tabs_list::TabListSnapshot) {
        self.catalog = snapshot.clone();
        if let Some(selected) = self.selected_context_id {
            for tab in &mut snapshot.tabs {
                tab.active = tab.id == selected;
            }
        }
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            let keys = self.item_keys();
            self.cursor.retain(&keys);
            self.settle_scroll(true);
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }

    /// Fact and status text can change rows' geometry; the list measures
    /// rows whose text changed when it next syncs. Republish, and optionally
    /// keep the active tab in view.
    fn remeasure(&mut self, reveal_active: bool) {
        self.revision = self.revision.saturating_add(1).max(1);
        self.settle_scroll(reveal_active);
    }

    fn select(&mut self, context_id: Option<Uuid>) {
        if self.selected_context_id != context_id {
            self.selected_context_id = context_id;
            self.replace_tabs(self.catalog.clone());
        }
    }
}

/// The next client-local selection. A bus without the channel, or one whose
/// channel closed, never yields; the catalog flags then stand.
async fn next_selection(slot: &mut Option<SelectionReceiver>) -> Option<Uuid> {
    loop {
        let Some(rx) = slot.as_mut() else {
            return std::future::pending().await;
        };
        if rx.changed().await.is_err() {
            *slot = None;
            continue;
        }
        return rx.borrow_and_update().context_id;
    }
}

type CompanionHandle = std::sync::Arc<Mutex<Option<CompanionState>>>;

#[derive(Default)]
pub struct SidebarPlugin;

impl Plugin for SidebarPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(|_, a0, a1| Self::activate(a0, a1))
            .blocking_services(|_, a0| Self::invoke_service(a0), None);
        for name in VisibilityCommand::NAMES.into_iter().chain(WIDTH_COMMANDS) {
            d.blocking_command(name, |_, _| Err(presentation_only_command()));
        }
    }
}

impl SidebarPlugin {
    fn activate(
        context: &NativeLifecycleContext,
        _tasks: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> std::result::Result<(), PluginCommandError> {
        // Validate configuration up front. The backend owns no presentation:
        // attach runtimes install their own `SidebarPresentation`.
        Settings::parse(context.settings.as_ref())?;
        Ok(())
    }

    fn invoke_service(context: &NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "presentation-input", "handle-input" => |event: AttachInputEvent, ctx| {
                Ok::<_, ServiceResponse>(handle_input(ctx, &event))
            },
        })
    }
}

/// Reached only when no sidebar presentation answered a visibility or width
/// command, for example from a shell or in a client without the sidebar
/// installed.
fn presentation_only_command() -> PluginCommandError {
    PluginCommandError::unavailable(
        "sidebar visibility and width commands only apply inside an attach client showing the sidebar",
    )
}

const fn input_endpoint_capability() -> &'static str {
    "bmux.sidebar.input"
}

fn input_endpoint() -> bmux_plugin::AttachInputEndpoint {
    bmux_plugin::AttachInputEndpoint {
        capability: input_endpoint_capability().to_string(),
        interface_id: "presentation-input".to_string(),
        operation: "handle-input".to_string(),
    }
}

/// An independently owned sidebar presentation. Hosts retain this value until
/// teardown and route geometry/input through the supplied registries.
pub struct SidebarPresentation {
    owner: CompanionHandle,
    layouts: std::sync::Arc<bmux_plugin::layout::PluginLayoutRegistry>,
    allocations: std::sync::Arc<bmux_plugin::layout::AllocationRegistry>,
    input: std::sync::Arc<bmux_plugin::AttachPresentationInputRegistry>,
}

impl SidebarPresentation {
    /// Install and start the sidebar for the invoking attach runtime.
    ///
    /// Facts come from the fact host in the runtime's shared state registry,
    /// where the runtime's other companions publish them, and remembered
    /// visibility from the runtime's local state directory.
    ///
    /// # Errors
    /// Returns configuration, subscription, publication or runtime errors.
    pub fn install_for_runtime(
        settings: Option<&toml::Value>,
        resources: &bmux_plugin::AttachPresentationResources,
    ) -> Result<Self, String> {
        let host = resources
            .states
            .get_or_insert_with(bmux_presentation_state::PresentationFactHostService::new);
        Self::install_with_facts(settings, resources, FactSource::Host(host))
    }

    /// Install and start an instance-owned frontend without global registries.
    ///
    /// Facts must be the registry used by this runtime's backend producer service.
    /// Layout, input and surfaces retain the existing compositor contract.
    ///
    /// # Errors
    /// Returns configuration, subscription, publication or runtime errors.
    pub fn install_scoped(
        settings: Option<&toml::Value>,
        resources: &bmux_plugin::AttachPresentationResources,
        facts: std::sync::Arc<bmux_presentation_state::PresentationFactRegistry>,
    ) -> Result<Self, String> {
        Self::install_with_facts(settings, resources, FactSource::Registry(facts))
    }

    fn install_with_facts(
        settings: Option<&toml::Value>,
        resources: &bmux_plugin::AttachPresentationResources,
        facts: FactSource,
    ) -> Result<Self, String> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|error| format!("sidebar companion requires an async runtime: {error}"))?;
        let (initial, mut tabs_rx) = resources
            .events
            .subscribe_state::<tabs_list::TabListSnapshot>(&tabs_list::STATE_KIND)
            .map_err(|error| format!("subscribing to tabs list: {error}"))?;
        let (initial_selection, mut selection_rx) = subscribe_selection(&resources.events);
        let mut facts_rx = facts.subscribe()?;
        let (initial_status, mut status_rx) = subscribe_tab_status(&resources.events)?;
        let (initial_appearance, mut appearance_rx) = subscribe_appearance(&resources.events);
        let presentation = Self::install_with_state(
            settings,
            resources.layouts.clone(),
            resources.allocations.clone(),
            resources.input.clone(),
            resources.surfaces.clone(),
            resources.plugin_local_state_dir(OWNER),
        )?;
        {
            let mut guard = presentation
                .owner
                .lock()
                .map_err(|_| "sidebar state lock poisoned".to_string())?;
            let companion = guard
                .as_mut()
                .ok_or_else(|| "sidebar companion is closed".to_string())?;
            companion.facts = Some(facts);
            companion.activity.replace(initial_status.as_ref().clone());
            if let Some(appearance) = &initial_appearance {
                companion.edge_colors = EdgeColors::from_appearance(appearance);
            }
            companion.selected_context_id = initial_selection;
            companion.replace_tabs(initial.as_ref().clone());
            publish_companion(companion)?;
            let owner = std::sync::Arc::downgrade(&presentation.owner);
            companion.subscription = Some(handle.spawn(async move {
                // Ages and working/done states change with time alone.
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let mut status = None;
                    let mut ticked = false;
                    let mut selection = None;
                    let mut appearance = None;
                    let tabs = tokio::select! {
                        result = tabs_rx.changed() => {
                            if result.is_err() { break; }
                            Some(tabs_rx.borrow_and_update().as_ref().clone())
                        }
                        context_id = next_selection(&mut selection_rx) => {
                            selection = Some(context_id);
                            None
                        }
                        result = facts_rx.changed() => {
                            if result.is_err() { break; }
                            facts_rx.borrow_and_update();
                            None
                        }
                        result = status_rx.changed() => {
                            if result.is_err() { break; }
                            status = Some(status_rx.borrow_and_update().as_ref().clone());
                            None
                        }
                        changed = next_appearance(&mut appearance_rx) => {
                            appearance = Some(changed);
                            None
                        }
                        _ = tick.tick() => {
                            ticked = true;
                            None
                        }
                    };
                    let Some(owner) = owner.upgrade() else {
                        break;
                    };
                    let Ok(mut guard) = owner.lock() else {
                        break;
                    };
                    let Some(companion) = guard.as_mut() else {
                        break;
                    };
                    if !companion.apply_changes(Changes {
                        tabs,
                        selection: selection.map(SelectionChange),
                        status,
                        appearance,
                        ticked,
                    }) {
                        continue;
                    }
                    if let Err(error) = publish_companion(companion) {
                        tracing::warn!(%error, "sidebar publication failed");
                    }
                }
            }));
            drop(guard);
        }
        Ok(presentation)
    }

    /// Install into presentation-local registries, without facts, a state
    /// subscription or remembered visibility. Callers feed snapshots through
    /// [`Self::publish`].
    ///
    /// # Errors
    /// Returns invalid settings or layout publication errors.
    pub fn install(
        settings: Option<&toml::Value>,
        layouts: std::sync::Arc<bmux_plugin::layout::PluginLayoutRegistry>,
        allocations: std::sync::Arc<bmux_plugin::layout::AllocationRegistry>,
        input: std::sync::Arc<bmux_plugin::AttachPresentationInputRegistry>,
        surfaces: std::sync::Arc<bmux_plugin::surface::PluginSurfaceRegistry>,
    ) -> Result<Self, String> {
        Self::install_with_state(settings, layouts, allocations, input, surfaces, None)
    }

    fn install_with_state(
        settings: Option<&toml::Value>,
        layouts: std::sync::Arc<bmux_plugin::layout::PluginLayoutRegistry>,
        allocations: std::sync::Arc<bmux_plugin::layout::AllocationRegistry>,
        input: std::sync::Arc<bmux_plugin::AttachPresentationInputRegistry>,
        surfaces: std::sync::Arc<bmux_plugin::surface::PluginSurfaceRegistry>,
        local_state_dir: Option<std::path::PathBuf>,
    ) -> Result<Self, String> {
        let settings = Settings::parse(settings).map_err(|error| error.to_string())?;
        let mut companion = CompanionState::new(settings, local_state_dir);
        companion.surfaces = Some(surfaces);
        companion.layouts = Some(layouts.clone());
        publish_layout(&companion)?;
        let owner = std::sync::Arc::new(Mutex::new(Some(companion)));
        register_input_callbacks(
            &owner,
            &|id, handler| allocations.register(id, handler),
            &|endpoint, handler| input.register(endpoint, handler),
        );
        register_visibility_commands(&owner, &input);
        register_width_commands(&owner, &input);
        register_focus_observer(&owner, &input);
        Ok(Self {
            owner,
            layouts,
            allocations,
            input,
        })
    }

    /// Apply the authoritative snapshot for this presentation.
    ///
    /// # Errors
    /// Returns state-lock or surface-publication errors.
    pub fn publish(&self, snapshot: tabs_list::TabListSnapshot) -> Result<(), String> {
        publish_for_installation(&self.owner, snapshot)
    }
}

impl Drop for SidebarPresentation {
    fn drop(&mut self) {
        self.allocations
            .remove(&PluginLayoutId::new(OWNER, LAYOUT_ID));
        self.input.remove(&input_endpoint());
        self.input.remove_commands(OWNER);
        if let Ok(mut owner) = self.owner.lock() {
            *owner = None;
        }
        self.layouts.remove_owner(OWNER);
    }
}

fn register_input_callbacks(
    owner: &CompanionHandle,
    allocation: &dyn Fn(PluginLayoutId, bmux_plugin::layout::AllocationHandler),
    input: &dyn Fn(bmux_plugin::AttachInputEndpoint, bmux_plugin::AttachPresentationInputHandler),
) {
    let allocation_owner = owner.clone();
    allocation(
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        std::sync::Arc::new(move |rect| {
            let Ok(mut guard) = allocation_owner.lock() else {
                return;
            };
            // A hidden sidebar's request allocates nothing; keep the last
            // shown geometry so showing it again restores scroll and layout.
            if let Some(companion) = guard.as_mut().filter(|companion| companion.visible())
                && let Err(error) = companion.publish_allocation(rect, publish_companion)
            {
                tracing::warn!(%error, "sidebar resize publication failed");
            }
        }),
    );
    let owner = owner.clone();
    input(
        input_endpoint(),
        std::sync::Arc::new(move |event| handle_local_input(&owner, event)),
    );
}

/// Follow the sidebar's keyboard focus in this client, to show it: the
/// heading lights up and the focused row is highlighted.
fn register_focus_observer(
    owner: &CompanionHandle,
    input: &bmux_plugin::AttachPresentationInputRegistry,
) {
    let owner = std::sync::Arc::downgrade(owner);
    input.register_focus_changed(
        input_endpoint(),
        std::sync::Arc::new(move |hook, focused| {
            let Some(owner) = owner.upgrade() else {
                return false;
            };
            let Ok(mut guard) = owner.lock() else {
                return false;
            };
            guard
                .as_mut()
                .is_some_and(|companion| companion.set_focus(hook, focused))
        }),
    );
}

/// Commands that step the sidebar's width in this runtime.
const WIDTH_COMMANDS: [&str; 2] = ["widen", "narrow"];

/// Answer `widen` and `narrow` in this runtime: like visibility, the width
/// one attach client shows is presentation state.
fn register_width_commands(
    owner: &CompanionHandle,
    input: &bmux_plugin::AttachPresentationInputRegistry,
) {
    for name in WIDTH_COMMANDS {
        let owner = std::sync::Arc::downgrade(owner);
        input.register_command(
            OWNER,
            name,
            std::sync::Arc::new(move |_| {
                let Some(owner) = owner.upgrade() else {
                    return AttachInputResult::default();
                };
                let Ok(mut guard) = owner.lock() else {
                    return AttachInputResult::default();
                };
                guard
                    .as_mut()
                    .map_or_else(AttachInputResult::default, |companion| {
                        step_width(companion, name == "widen")
                    })
            }),
        );
    }
}

/// Widen or narrow the sidebar by [`WIDTH_STEP`] cells and remember it.
fn step_width(companion: &mut CompanionState, widen: bool) -> AttachInputResult {
    let width = if widen {
        companion.width.saturating_add(WIDTH_STEP)
    } else {
        companion.width.saturating_sub(WIDTH_STEP)
    };
    let changed = companion.set_width(width);
    if changed {
        // The edge moved out from under the pointer; the next move re-hovers it.
        companion.engaged.edge_hovered = false;
    }
    let dirty = changed && companion.visible() && republish_width(companion, true);
    let status_message = if changed {
        companion.remember_width().err()
    } else {
        None
    };
    AttachInputResult {
        consumed: true,
        dirty,
        status_message,
        ..AttachInputResult::default()
    }
}

/// Answer `toggle`, `show` and `hide` in this runtime: what one attach client
/// shows is presentation state, not plugin state.
fn register_visibility_commands(
    owner: &CompanionHandle,
    input: &bmux_plugin::AttachPresentationInputRegistry,
) {
    for name in VisibilityCommand::NAMES {
        let Some(command) = VisibilityCommand::from_name(name) else {
            continue;
        };
        let owner = std::sync::Arc::downgrade(owner);
        input.register_command(
            OWNER,
            name,
            std::sync::Arc::new(move |_| {
                let Some(owner) = owner.upgrade() else {
                    return AttachInputResult::default();
                };
                let Ok(mut guard) = owner.lock() else {
                    return AttachInputResult::default();
                };
                guard
                    .as_mut()
                    .map_or_else(AttachInputResult::default, |companion| {
                        apply_visibility(companion, command)
                    })
            }),
        );
    }
}

fn apply_visibility(
    companion: &mut CompanionState,
    command: VisibilityCommand,
) -> AttachInputResult {
    let (changed, status_message) = match companion.visibility.apply(command) {
        Ok(changed) => (changed, None),
        // The new state still applies to this client; say it was not kept.
        Err(error) => (true, Some(error)),
    };
    if changed {
        if !companion.visible() {
            companion.cursor.set_hovered(None);
        }
        companion.revision = companion.revision.saturating_add(1).max(1);
        companion.presentation_pending.set(true);
        if let Err(error) = publish_layout(companion).and_then(|()| publish_companion(companion)) {
            tracing::warn!(%error, "sidebar visibility publication failed");
        }
    }
    AttachInputResult {
        consumed: true,
        dirty: changed,
        status_message,
        ..AttachInputResult::default()
    }
}

fn layout_request(settings: &Settings, width: u16, visible: bool) -> PluginLayoutRequest {
    let id = PluginLayoutId::new(OWNER, LAYOUT_ID);
    if !visible {
        return PluginLayoutRequest::hidden(id, settings.order);
    }
    PluginLayoutRequest::split(
        id,
        settings.order,
        match settings.placement {
            Placement::Left => LayoutEdge::Left,
            Placement::Right => LayoutEdge::Right,
        },
        LayoutExtent::Responsive {
            preferred: width,
            collapsed: settings.collapsed_width,
            collapse_below: settings.collapse_below_width,
        },
    )
}

/// Publish the layout request for the companion's current visibility.
fn publish_layout(companion: &CompanionState) -> Result<(), String> {
    let Some(layouts) = companion.layouts.as_deref() else {
        return Ok(());
    };
    let request = layout_request(&companion.settings, companion.width, companion.visible());
    if layouts.requests().contains(&request) {
        return Ok(());
    }
    layouts
        .publish(
            OWNER,
            PluginLayoutSnapshot {
                revision: layouts.owner_revision(OWNER).unwrap_or(0).saturating_add(1),
                requests: vec![request],
            },
        )
        .map_err(|error| format!("publishing sidebar layout: {error:?}"))?;
    Ok(())
}

fn publish_for_installation(
    owner: &CompanionHandle,
    snapshot: tabs_list::TabListSnapshot,
) -> Result<(), String> {
    let mut guard = owner
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())?;
    if let Some(companion) = guard.as_mut() {
        companion.replace_tabs(snapshot);
        companion.presentation_pending.set(true);
        return publish_companion(companion);
    }
    drop(guard);
    Ok(())
}

/// A field's selection, semantic and text id: short, since every selectable
/// word repeats it and the surface's metadata is bounded. Positions are unique
/// within a surface, which is all the ids must be.
fn selection_id(index: usize, field: &str) -> String {
    format!("{index}:{}", field.get(..1).unwrap_or(field))
}

fn publish_surfaces(
    registry: &bmux_plugin::surface::PluginSurfaceRegistry,
    revision: u64,
    mut surfaces: Vec<PluginSurface>,
) -> Result<(), String> {
    let Err(error) = registry.publish_advancing(
        OWNER,
        PluginSurfaceSnapshot {
            revision,
            surfaces: surfaces.clone(),
        },
    ) else {
        return Ok(());
    };
    // Text selection is the part of the surface that grows with what it
    // shows; drawing the tabs without it beats drawing nothing.
    if surfaces.iter().all(|surface| surface.metadata.is_none()) {
        return Err(format!("publishing sidebar surface: {error:?}"));
    }
    tracing::warn!(
        ?error,
        "sidebar surface rejected; publishing it without text selection"
    );
    for surface in &mut surfaces {
        surface.metadata = None;
    }
    registry
        .publish_advancing(OWNER, PluginSurfaceSnapshot { revision, surfaces })
        .map_err(|error| format!("publishing sidebar surface: {error:?}"))?;
    Ok(())
}

fn publish_companion(companion: &CompanionState) -> Result<(), String> {
    let revision = companion.revision.max(1);
    // A companion publishes only into the registry of the runtime that
    // installed it; an uninstalled one has nowhere to publish.
    let Some(registry) = companion.surfaces.as_deref() else {
        // Nowhere to publish: nothing is waiting to be published either.
        companion.presentation_pending.set(false);
        return Ok(());
    };
    // A hidden sidebar keeps its identity and state but draws nothing.
    let (surfaces, hits) = if companion.visible() {
        let (surface, hits) = build_surface_with_hits(companion, revision);
        (vec![surface], hits)
    } else {
        (Vec::new(), HitTable::new())
    };
    publish_surfaces(registry, revision, surfaces)?;
    // Publishing may rebase the revision; input arrives with the applied one.
    let applied = registry
        .owner_snapshot(OWNER)
        .map_or(revision, |snapshot| snapshot.revision);
    companion.hits.borrow_mut().publish(applied, hits);
    companion.presentation_pending.set(false);
    Ok(())
}

fn tab_fact(
    tab: &tabs_list::TabListEntry,
    facts: &bmux_presentation_state::PresentationFactRegistry,
) -> Option<PresentationFact> {
    let entity = PresentationEntityRef::new("bmux.tabs", tab.id.to_string());
    facts
        .facts_for_entity(&entity)
        .into_iter()
        .map(|(_, fact)| fact)
        .max_by(|left, right| {
            (left.priority, role_rank(left.role), &left.key).cmp(&(
                right.priority,
                role_rank(right.role),
                &right.key,
            ))
        })
}

const fn role_rank(role: PresentationFactRole) -> u8 {
    match role {
        PresentationFactRole::Neutral => 0,
        PresentationFactRole::Idle => 1,
        PresentationFactRole::Active => 2,
        PresentationFactRole::Success => 3,
        PresentationFactRole::Warning => 4,
        PresentationFactRole::Attention => 5,
        PresentationFactRole::Error => 6,
    }
}

const fn fact_style(role: PresentationFactRole, fallback: RenderStyle) -> RenderStyle {
    match role {
        PresentationFactRole::Neutral => fallback,
        PresentationFactRole::Idle => fallback.dim(),
        PresentationFactRole::Active => fallback.named_foreground(RenderNamedColor::BrightCyan),
        PresentationFactRole::Success => fallback.named_foreground(RenderNamedColor::BrightGreen),
        PresentationFactRole::Warning => fallback.named_foreground(RenderNamedColor::BrightYellow),
        PresentationFactRole::Attention => {
            fallback.named_foreground(RenderNamedColor::BrightMagenta)
        }
        PresentationFactRole::Error => fallback.named_foreground(RenderNamedColor::BrightRed),
    }
}

fn render_template(
    template: &str,
    tab: &tabs_list::TabListEntry,
    index: usize,
    show_index: bool,
    fact: Option<&PresentationFact>,
    activity: Option<&activity::TabActivity>,
) -> String {
    const MARKER_TOKEN: &str = concat!("{", "marker}");
    const INDEX_TOKEN: &str = concat!("{", "index}");
    const FACT_TOKEN: &str = concat!("{", "fact}");
    let marker = if tab.active { "●" } else { "○" };
    let index = if show_index {
        format!("{} ", index.saturating_add(1))
    } else {
        String::new()
    };
    template
        .replace("{{", "\u{0}")
        .replace("}}", "\u{1}")
        .replace(MARKER_TOKEN, marker)
        .replace(INDEX_TOKEN, &index)
        .replace("{name}", &tab.name)
        .replace("{id}", &tab.id.to_string())
        .replace("{active}", if tab.active { "active" } else { "idle" })
        .replace(FACT_TOKEN, fact.map_or("", |fact| fact.short_text.as_str()))
        .replace(
            "{fact_detail}",
            fact.and_then(|fact| fact.detail_text.as_deref())
                .unwrap_or(""),
        )
        .replace(
            "{fact_icon}",
            fact.and_then(|fact| fact.icon_id.as_deref()).unwrap_or(""),
        )
        .replace("{state_icon}", &state_glyph(activity).0.to_string())
        .replace(
            "{state}",
            activity.map_or("", |activity| activity.state.label()),
        )
        .replace(
            "{command}",
            activity
                .and_then(|activity| activity.command.as_deref())
                .unwrap_or(""),
        )
        .replace(
            "{cwd}",
            activity
                .and_then(|activity| activity.cwd.as_deref())
                .unwrap_or(""),
        )
        .replace(
            "{branch}",
            activity
                .and_then(|activity| activity.branch.as_deref())
                .unwrap_or(""),
        )
        .replace(
            "{detail}",
            activity.map_or("", |activity| activity.detail.as_str()),
        )
        .replace('\u{0}', "{")
        .replace('\u{1}', "}")
}

/// A tab's state glyph and color. They show only what the tab is doing; the
/// card's tint and accent bar show which tab is selected.
fn state_glyph(activity: Option<&activity::TabActivity>) -> (char, RenderColor) {
    let state = activity.map_or(activity::TabState::Idle, |activity| activity.state);
    (state.glyph(), state.color())
}

/// The sidebar's colors: the terminal's own background, a tinted card for
/// the active and hovered tabs, muted secondary text and an accent bar.
#[derive(Debug, Clone, Copy)]
struct SidebarPalette {
    base: RenderStyle,
    heading: RenderStyle,
    /// The heading while the sidebar has the keyboard.
    focused_heading: RenderStyle,
    title: RenderStyle,
    active_title: RenderStyle,
    muted: RenderStyle,
    accent: RenderStyle,
    active_card: RenderStyle,
    hover_card: RenderStyle,
}

impl Default for SidebarPalette {
    fn default() -> Self {
        Self {
            base: RenderStyle::new(),
            heading: RenderStyle::new()
                .named_foreground(RenderNamedColor::BrightBlack)
                .bold(),
            focused_heading: RenderStyle::new()
                .named_foreground(RenderNamedColor::Blue)
                .bold()
                .underline(),
            title: RenderStyle::new().named_foreground(RenderNamedColor::White),
            active_title: RenderStyle::new()
                .named_foreground(RenderNamedColor::BrightWhite)
                .bold(),
            muted: RenderStyle::new().named_foreground(RenderNamedColor::BrightBlack),
            accent: RenderStyle::new().named_foreground(RenderNamedColor::Blue),
            active_card: RenderStyle::new().background(RenderColor::Indexed(236)),
            hover_card: RenderStyle::new().background(RenderColor::Indexed(235)),
        }
    }
}

/// Paint text over a card: its background, keeping the text's own colors.
trait OverCard {
    fn background_of(self, card: RenderStyle) -> Self;
}

impl OverCard for RenderStyle {
    fn background_of(mut self, card: RenderStyle) -> Self {
        if card.bg.is_some() {
            self.bg = card.bg;
        }
        self
    }
}

/// The owner-local region id of the resize edge.
const RESIZE_EDGE_REGION: &str = "resize-edge";

/// The column facing the panes, over the sidebar's height (its content's,
/// `content_rows`, when it is content-sized); none when the sidebar is too
/// narrow to spare one.
fn resize_edge(state: &CompanionState, content_rows: u16) -> Option<ExtensionRect> {
    let width = state.allocated_width();
    if width < 3 {
        return None;
    }
    let height = if state.settings.content_height {
        content_rows.saturating_add(1)
    } else {
        state
            .allocation
            .map_or_else(|| content_rows.saturating_add(1), |rect| rect.h)
    };
    let x = match state.settings.placement {
        Placement::Left => width - 1,
        Placement::Right => 0,
    };
    Some(ExtensionRect::new(x, 0, 1, height.max(1)))
}

/// What a group's tabs are doing, as `◆1 ◉2 ■3`-style counts in their
/// states' colors, most pressing first, within `room` cells; the tab count
/// when none is doing anything.
fn group_summary(
    state: &CompanionState,
    tabs: &[usize],
    room: usize,
) -> Vec<(String, Option<RenderColor>)> {
    let mut counts = std::collections::BTreeMap::<activity::TabState, usize>::new();
    for index in tabs {
        let tab = &state.snapshot.tabs[*index];
        if let Some(activity) = state.activity.activity(tab.id, tab.active)
            && activity.state != activity::TabState::Idle
        {
            *counts.entry(activity.state).or_default() += 1;
        }
    }
    let mut parts = Vec::new();
    let mut used = 0;
    for (tab_state, count) in counts {
        let text = format!("{}{count}", tab_state.glyph());
        let cells = bmux_cell_geometry::display_width(&text) + usize::from(!parts.is_empty());
        if used + cells > room {
            break;
        }
        used += cells;
        parts.push((text, Some(tab_state.color())));
    }
    if parts.is_empty() {
        let count = tabs.len().to_string();
        if count.len() <= room {
            parts.push((count, None));
        }
    }
    parts
}
#[cfg(test)]
fn build_surface(state: &CompanionState, revision: u64) -> PluginSurface {
    build_surface_with_hits(state, revision).0
}

// Paint and logical descriptions share the same measured scroll projection.

/// The sidebar's interactive regions from the regions its components
/// painted: a region per visible row (keyed in `hits`, and recorded in
/// `placed`) and the resize edge.
fn sidebar_regions(
    painted: Vec<PluginSurfaceRegion>,
    hits: &mut HitTable<SidebarHit>,
    placed: &mut std::collections::BTreeSet<SidebarHit>,
    drag: Option<ColumnGesture>,
) -> Vec<PluginSurfaceRegion> {
    let mut regions = Vec::new();
    for hit in painted {
        // The list registers each row for keyboard focus too, under the same
        // id; the pointer regions are the interactive ones.
        if hit
            .interaction
            .as_ref()
            .is_some_and(|interaction| !interaction.pointer_events)
        {
            continue;
        }
        let item = if hit.local_id == view::RESIZE_EDGE {
            Some(SidebarHit::ResizeEdge)
        } else if hit.local_id == view::SCROLLBAR {
            Some(SidebarHit::Scrollbar)
        } else {
            hit.local_id
                .strip_prefix(view::LIST_ID)
                .and_then(|rest| rest.strip_prefix(".item."))
                .and_then(region_item)
        };
        let Some(item) = item.filter(|item| placed.insert(*item)) else {
            continue;
        };
        let cursor = match item {
            SidebarHit::ResizeEdge => bmux_plugin::surface::PluginSurfaceCursor::ResizeHorizontal,
            SidebarHit::Scrollbar => thumb_cursor(drag),
            SidebarHit::Tab(_) | SidebarHit::Group(_) => {
                bmux_plugin::surface::PluginSurfaceCursor::Pointer
            }
        };
        let region = PluginSurfaceRegion::new(item_region_id(item), hit.rect)
            .endpoint(input_endpoint())
            .focusable(cursor)
            .key(hits.key(item));
        // Rows take the keyboard under the pointer, like panes, where the
        // user has focus-on-hover; the resize edge never does.
        regions.push(match item {
            SidebarHit::ResizeEdge | SidebarHit::Scrollbar => region,
            SidebarHit::Tab(_) | SidebarHit::Group(_) => region.focus_on_hover(),
        });
    }
    regions
}

/// The plain text of the fields on screen, by selection scope: exactly the
/// text each painted field's selection indexes into.
fn visible_field_text(
    rows: &[(SidebarHit, view::RowView)],
    placed: &std::collections::BTreeSet<SidebarHit>,
    width: u16,
) -> std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String> {
    let mut text = std::collections::BTreeMap::new();
    for (key, row) in rows {
        let view::RowView::Card(card) = row else {
            continue;
        };
        if !placed.contains(key) {
            continue;
        }
        for (field, value) in card.fields.iter().zip(card.field_texts(u64::from(width))) {
            if !value.trim().is_empty() {
                text.insert(field.id.clone().into(), value);
            }
        }
    }
    text
}

/// The sidebar's surface and what its regions' keys mean: the heading, the
/// rows (painted, clipped and given hit regions by the list component), and
/// the resize edge.
fn build_surface_with_hits(
    state: &CompanionState,
    revision: u64,
) -> (PluginSurface, HitTable<SidebarHit>) {
    use bmux_plugin::render_paint::tui_style;
    let mut hits = HitTable::new();
    let width = state.allocated_width();
    let palette = SidebarPalette::default();
    let rows = state.row_views(revision);
    let keys = rows.iter().map(|(key, _)| *key).collect::<Vec<_>>();
    let list = state.synced_list(&rows);
    let list_height = state.list_height(&keys);
    let (list_columns, scrollbar_x) = state.list_columns(list_height);
    let shown_rows = {
        let list_state = state.list.borrow();
        u16::try_from(
            list_state
                .total_height()
                .saturating_sub(list_state.scroll.vertical_offset())
                .min(u64::from(list_height)),
        )
        .unwrap_or(u16::MAX)
    };
    let height = if state.settings.content_height {
        shown_rows.saturating_add(2)
    } else {
        state
            .allocation
            .map_or_else(|| list_height.saturating_add(2), |rect| rect.h)
    }
    .max(1);
    let count = state.tabs_in_scope();
    let edge = resize_edge(state, shown_rows).map(|edge| view::Edge {
        x: edge.x,
        height: edge.h.min(height),
        highlighted: state.engaged.edge_hovered || state.resize.is_some(),
        style: tui_style(state.edge_colors.hover.background_of(palette.base)),
    });
    let component = view::SidebarView {
        size: (width, height),
        base: tui_style(palette.base),
        heading: view::styled(
            state.settings.heading.to_uppercase(),
            // Lit while the sidebar has the keyboard.
            tui_style(if state.engaged.focused {
                palette.focused_heading
            } else {
                palette.heading
            }),
        ),
        count: view::styled(count.to_string(), tui_style(palette.muted)),
        list,
        state: &state.list,
        list_height,
        list_columns,
        scrollbar: scrollbar_x.map(|x| scrollbar_view(state, x, list_height, &palette)),
        edge,
    };
    let size = bmux_tui::geometry::Size::new(width, height);
    let layout = bmux_tui::component::Component::layout(
        &component,
        bmux_tui::component::Constraints::tight(size),
        &mut bmux_tui::component::LayoutCx::new(),
    );
    let painted = bmux_plugin::component_viewport::ComponentViewport::new(
        layout,
        bmux_tui::geometry::Rect::new(0, 0, width, height),
        bmux_tui::geometry::Point::new(0, 0),
    )
    .map(|viewport| viewport.paint(&component));
    let Some(painted) = painted else {
        let surface = PluginSurface::layout(
            PluginSurfaceId::new(OWNER, SURFACE_ID, RETAINED_ID),
            revision,
            PluginLayoutId::new(OWNER, LAYOUT_ID),
            Vec::new(),
        )
        .opaque(true);
        return (surface, hits);
    };
    let mut surface = PluginSurface::layout(
        PluginSurfaceId::new(OWNER, SURFACE_ID, RETAINED_ID),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        bmux_plugin::component_render::buffer_render_ops(&painted.buffer),
    )
    .opaque(true);
    let mut placed = std::collections::BTreeSet::new();
    let drag = state.resize.map(|drag| drag.mode);
    for region in sidebar_regions(painted.hits, &mut hits, &mut placed, drag) {
        surface = surface.interactive_region(region);
    }
    let text = visible_field_text(&rows, &placed, width);
    surface.metadata = Some(bmux_plugin::surface::PluginSurfaceMetadata {
        selection: Some(painted.selection),
        semantics: Some(painted.semantics),
        text,
    });
    (surface, hits)
}

/// This client's selection changed, to this tab (or none).
struct SelectionChange(Option<Uuid>);

/// What changed since the sidebar last looked: a new tab list, this
/// client's selection, tab status or appearance, or only time passing.
struct Changes {
    tabs: Option<tabs_list::TabListSnapshot>,
    selection: Option<SelectionChange>,
    status: Option<tabs_status::TabStatusSnapshot>,
    appearance: Option<bmux_appearance::RuntimeAppearance>,
    ticked: bool,
}

impl CompanionState {
    /// Take `changes`; returns whether the sidebar must be republished.
    fn apply_changes(&mut self, changes: Changes) -> bool {
        let Changes {
            tabs,
            selection,
            status,
            appearance,
            ticked,
        } = changes;
        if let Some(active) = self.snapshot.tabs.iter().find(|tab| tab.active) {
            self.activity.mark_viewed(active.id);
        }
        if let Some(appearance) = &appearance {
            self.edge_colors = EdgeColors::from_appearance(appearance);
        }
        if ticked && !self.activity.time_sensitive() {
            return false;
        }
        let status_changed = status.is_some();
        if let Some(status) = status {
            self.activity.replace(status);
        }
        if let Some(tabs) = tabs {
            self.replace_tabs(tabs);
        } else if let Some(SelectionChange(context_id)) = selection {
            self.select(context_id);
        } else {
            // Only a fact change brings the active tab back into
            // view; status updates and ticks keep the scroll.
            self.remeasure(!ticked && !status_changed);
        }
        self.presentation_pending.set(true);
        true
    }
}

/// Follow the tabs plugin's forwarded `tabs-status` state on this runtime's
/// bus, registering its decoder when nothing has yet.
type SelectionReceiver = tokio::sync::watch::Receiver<
    std::sync::Arc<bmux_tabs_plugin_api::tabs_local_view::TabSelection>,
>;

/// This attachment's client-local selection (`tabs-local-view`) and its
/// receiver; both absent on buses that do not publish it.
fn subscribe_selection(
    events: &std::sync::Arc<bmux_plugin::EventBus>,
) -> (Option<Uuid>, Option<SelectionReceiver>) {
    events
        .subscribe_state::<bmux_tabs_plugin_api::tabs_local_view::TabSelection>(
            &bmux_tabs_plugin_api::tabs_local_view::STATE_KIND,
        )
        .ok()
        .map_or((None, None), |(selection, rx)| {
            (selection.context_id, Some(rx))
        })
}

type AppearanceReceiver =
    tokio::sync::watch::Receiver<std::sync::Arc<bmux_appearance::RuntimeAppearance>>;

/// The runtime's appearance and its changes, on buses that carry it.
fn subscribe_appearance(
    events: &std::sync::Arc<bmux_plugin::EventBus>,
) -> (
    Option<bmux_appearance::RuntimeAppearance>,
    Option<AppearanceReceiver>,
) {
    let kind = bmux_appearance::RUNTIME_APPEARANCE_STATE_KIND;
    // Installed before the runtime first publishes it: register the channel,
    // so that publication still arrives.
    if events
        .subscribe_state::<bmux_appearance::RuntimeAppearance>(&kind)
        .is_err()
    {
        events.register_state_channel_with_decoder(
            kind.clone(),
            bmux_appearance::RuntimeAppearance::default(),
        );
    }
    events
        .subscribe_state::<bmux_appearance::RuntimeAppearance>(&kind)
        .ok()
        .map_or((None, None), |(appearance, rx)| {
            (Some(appearance.as_ref().clone()), Some(rx))
        })
}

/// The next appearance. A bus without the channel, or one whose channel
/// closed, never yields.
async fn next_appearance(
    slot: &mut Option<AppearanceReceiver>,
) -> bmux_appearance::RuntimeAppearance {
    loop {
        let Some(rx) = slot.as_mut() else {
            return std::future::pending().await;
        };
        if rx.changed().await.is_err() {
            *slot = None;
            continue;
        }
        return rx.borrow_and_update().as_ref().clone();
    }
}

fn subscribe_tab_status(
    events: &std::sync::Arc<bmux_plugin::EventBus>,
) -> Result<
    (
        std::sync::Arc<tabs_status::TabStatusSnapshot>,
        tokio::sync::watch::Receiver<std::sync::Arc<tabs_status::TabStatusSnapshot>>,
    ),
    String,
> {
    if let Ok(subscription) =
        events.subscribe_state::<tabs_status::TabStatusSnapshot>(&tabs_status::STATE_KIND)
    {
        return Ok(subscription);
    }
    events.register_state_channel_with_decoder(
        tabs_status::STATE_KIND,
        tabs_status::TabStatusSnapshot {
            tabs: Vec::new(),
            now_ms: 0,
            revision: 0,
        },
    );
    events
        .subscribe_state::<tabs_status::TabStatusSnapshot>(&tabs_status::STATE_KIND)
        .map_err(|error| format!("subscribing to tab status: {error}"))
}

/// What an event is addressed to: through the hit table of the revision it
/// was published with when it carries a typed key (a retired revision
/// resolves to nothing), otherwise through its owner-local region id.
fn hit(companion: &CompanionState, event: &AttachInputEvent) -> Option<SidebarHit> {
    let target = event.target.as_ref()?;
    target.key.map_or_else(
        || region_item(&target.region),
        |key| {
            companion
                .hits
                .borrow()
                .resolve(target.revision, key)
                .copied()
        },
    )
}

/// The list's scrollbar at column `x`, colored as the appearance colors
/// scrollbars: a subtle track and an accent thumb, both lit while the
/// column is hovered or the thumb dragged.
fn scrollbar_view(
    state: &CompanionState,
    x: u16,
    list_height: u16,
    _palette: &SidebarPalette,
) -> view::ScrollbarView {
    use bmux_plugin::render_paint::tui_style;
    let colors = state.edge_colors;
    let lit = state.engaged.edge_hovered || state.resize.is_some();
    view::ScrollbarView {
        x,
        state: state.list.borrow().scrollbar_state(u64::from(list_height)),
        policy: scrollbar_policy(state.settings.placement, state.engaged.focused || lit),
        styles: bmux_tui_components::scrollbar::ScrollbarStyles {
            begin: tui_style(if lit { colors.hover } else { colors.track }),
            track: tui_style(if lit { colors.hover } else { colors.track }),
            thumb: tui_style(
                if state
                    .resize
                    .is_some_and(|drag| drag.mode == ColumnGesture::Scroll)
                {
                    colors.hover
                } else {
                    colors.thumb
                },
            ),
            end: tui_style(if lit { colors.hover } else { colors.track }),
        },
    }
}

/// The scrollbar's glyphs: a full-width thumb on a centered track while the
/// sidebar has the keyboard or the pointer is on its column, otherwise a slim
/// bar against the panes that stays out of the way.
const fn scrollbar_policy(
    placement: Placement,
    wide: bool,
) -> bmux_tui_components::scrollbar::ScrollbarPolicy {
    use bmux_tui_components::scrollbar::ScrollbarPolicy;
    if wide {
        return ScrollbarPolicy::vertical();
    }
    // The column faces the panes on the sidebar's far side.
    let (track, thumb) = match placement {
        Placement::Left => ("▕", "▐"),
        Placement::Right => ("▏", "▌"),
    };
    ScrollbarPolicy::vertical().symbols(track, track, thumb, track)
}

/// The colors of the column facing the panes: the scrollbar's track and
/// thumb, and the column lit under the pointer. They follow the runtime's
/// appearance as Bcode's lists do: the inactive border for the track, the
/// cursor (accent) for the thumb, and the active border when lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeColors {
    track: RenderStyle,
    thumb: RenderStyle,
    hover: RenderStyle,
}

impl Default for EdgeColors {
    fn default() -> Self {
        Self {
            track: RenderStyle::new().named_foreground(RenderNamedColor::BrightBlack),
            thumb: RenderStyle::new().named_foreground(RenderNamedColor::Blue),
            hover: RenderStyle::new().named_foreground(RenderNamedColor::BrightBlue),
        }
    }
}

impl EdgeColors {
    /// The colors `appearance` gives them; a color it does not give keeps
    /// its default.
    fn from_appearance(appearance: &bmux_appearance::RuntimeAppearance) -> Self {
        let fallback = Self::default();
        let color = |value: &str, fallback: RenderStyle| {
            value.parse::<bmux_color::Rgb>().map_or(fallback, |rgb| {
                RenderStyle::new().foreground(RenderColor::Rgb {
                    r: rgb.r,
                    g: rgb.g,
                    b: rgb.b,
                })
            })
        };
        Self {
            track: color(&appearance.border.inactive, fallback.track),
            thumb: color(&appearance.cursor, fallback.thumb),
            hover: color(&appearance.border.active, fallback.hover),
        }
    }
}

/// Press, drag and release on the column facing the panes (the resize edge,
/// carrying the scrollbar's thumb while the rows overflow). The first
/// movement decides: sideways resizes (within the minimum and maximum, and
/// remembered on release); up or down from the thumb scrolls, from where it
/// was grabbed; released without moving on the track, the list jumps there.
fn update_resize(owner: &CompanionHandle, event: &AttachInputEvent) -> Option<AttachInputResult> {
    if event.event_kind != "pointer" {
        return None;
    }
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    // Where the pointer is on the screen: a drag past the sidebar's own
    // edge has no surface column.
    let screen_col = event.screen_col.or_else(|| {
        let x = companion.allocation.map_or(0, |rect| rect.x);
        event.col.map(|col| x.saturating_add(col))
    })?;
    let row = companion.track_row(event).unwrap_or(0);
    let result = match (event.phase.as_str(), companion.resize) {
        ("down", None)
            if event.button.as_deref() == Some("left")
                && matches!(
                    hit(companion, event),
                    Some(SidebarHit::ResizeEdge | SidebarHit::Scrollbar)
                ) =>
        {
            let grab = (hit(companion, event) == Some(SidebarHit::Scrollbar))
                .then(|| companion.scrollbar_geometry())
                .flatten()
                .map(|(_, layout)| row - i32::from(layout.thumb_start));
            companion.resize = Some(ResizeDrag {
                start_col: screen_col,
                start_width: companion.width,
                start_row: row,
                grab,
                mode: ColumnGesture::Undecided,
            });
            AttachInputResult {
                consumed: true,
                preserve_focus: true,
                capture_pointer: true,
                // Republishing while dragging bumps the revision; keep the drag.
                preserve_capture_across_revisions: true,
                dirty: republish_width(companion, false),
                ..AttachInputResult::default()
            }
        }
        ("drag", Some(drag)) => AttachInputResult {
            consumed: true,
            preserve_focus: true,
            dirty: drag_column(companion, drag, screen_col, row),
            ..AttachInputResult::default()
        },
        ("up", Some(drag)) => {
            companion.resize = None;
            let status_message = match drag.mode {
                ColumnGesture::Resize => companion.remember_width().err(),
                ColumnGesture::Undecided if drag.grab.is_none() => {
                    // A click on the track jumps there.
                    if let Some((_, layout)) = companion.scrollbar_geometry()
                        && (0..i32::from(layout.track_len)).contains(&row)
                    {
                        companion.scroll_thumb_to(layout, row - i32::from(layout.thumb_len) / 2);
                    }
                    None
                }
                ColumnGesture::Undecided | ColumnGesture::Scroll => None,
            };
            AttachInputResult {
                consumed: true,
                preserve_focus: true,
                release_capture: true,
                dirty: republish_width(companion, false),
                status_message,
                ..AttachInputResult::default()
            }
        }
        // The drag owns the pointer until it is released.
        (_, Some(_)) => AttachInputResult {
            consumed: true,
            preserve_focus: true,
            ..AttachInputResult::default()
        },
        _ => return None,
    };
    drop(guard);
    Some(result)
}

/// One step of a drag on the column facing the panes: decide what it is by
/// its first movement, then resize or scroll. Returns whether anything
/// changed.
fn drag_column(
    companion: &mut CompanionState,
    mut drag: ResizeDrag,
    screen_col: u16,
    row: i32,
) -> bool {
    let sideways = i32::from(screen_col) - i32::from(drag.start_col);
    if drag.mode == ColumnGesture::Undecided {
        if sideways != 0 {
            drag.mode = ColumnGesture::Resize;
        } else if row != drag.start_row && drag.grab.is_some() {
            drag.mode = ColumnGesture::Scroll;
        }
    }
    companion.resize = Some(drag);
    match drag.mode {
        ColumnGesture::Resize => {
            let width = match companion.settings.placement {
                Placement::Left => i32::from(drag.start_width) + sideways,
                Placement::Right => i32::from(drag.start_width) - sideways,
            };
            let width = u16::try_from(width.max(0)).unwrap_or(u16::MAX);
            companion.set_width(width) && republish_width(companion, true)
        }
        ColumnGesture::Scroll => {
            if let (Some(grab), Some((_, layout))) = (drag.grab, companion.scrollbar_geometry()) {
                companion.scroll_thumb_to(layout, row - grab);
            }
            republish_width(companion, false)
        }
        ColumnGesture::Undecided => false,
    }
}

/// Republish after a width or resize-edge change; `layout` when the width
/// changed and the host must allocate it. Returns whether anything changed.
fn republish_width(companion: &mut CompanionState, layout: bool) -> bool {
    companion.revision = companion.revision.saturating_add(1).max(1);
    let published = if layout {
        companion.presentation_pending.set(true);
        publish_layout(companion).and_then(|()| publish_companion(companion))
    } else {
        publish_companion(companion)
    };
    if let Err(error) = &published {
        tracing::warn!(%error, "sidebar resize publication failed");
    }
    published.is_ok()
}

fn update_hover(owner: &CompanionHandle, event: &AttachInputEvent) -> bool {
    if !matches!(event.phase.as_str(), "enter" | "move" | "leave") {
        return false;
    }
    let Ok(mut guard) = owner.lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    let hovered = if event.phase == "leave" {
        None
    } else {
        hit(companion, event)
    };
    let previous = companion.cursor.clone();
    let previous_edge = companion.engaged.edge_hovered;
    // The column facing the panes lights up anywhere on it, thumb included.
    companion.engaged.edge_hovered = matches!(
        hovered,
        Some(SidebarHit::ResizeEdge | SidebarHit::Scrollbar)
    );
    let row = hovered.filter(|hit| matches!(hit, SidebarHit::Tab(_) | SidebarHit::Group(_)));
    if !companion.cursor.set_hovered(row) && previous_edge == companion.engaged.edge_hovered {
        return false;
    }
    companion.revision = companion.revision.saturating_add(1).max(1);
    if publish_companion(companion).is_err() {
        companion.cursor = previous;
        companion.engaged.edge_hovered = previous_edge;
        return false;
    }
    true
}

fn update_scroll(owner: &CompanionHandle, event: &AttachInputEvent) -> bool {
    if event.phase != "wheel" || event.wheel_delta == 0 {
        return false;
    }
    let Ok(mut guard) = owner.lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    let previous_scroll = companion.list.borrow().scroll;
    if !companion.scroll_rows(if event.wheel_delta > 0 { -1 } else { 1 }) {
        return true;
    }
    companion.revision = companion.revision.saturating_add(1).max(1);
    if publish_companion(companion).is_err() {
        companion.list.borrow_mut().scroll = previous_scroll;
        return false;
    }
    true
}

fn update_keyboard(owner: &CompanionHandle, event: &AttachInputEvent) -> Option<AttachInputResult> {
    if event.event_kind != "key" || !matches!(event.phase.as_str(), "press" | "repeat") {
        return None;
    }
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    let target = hit(companion, event).filter(|hit| *hit != SidebarHit::ResizeEdge)?;
    let input = match event.key.as_deref()? {
        "up" => CursorInput::Up,
        "down" => CursorInput::Down,
        "home" => CursorInput::Home,
        "end" => CursorInput::End,
        // Escape hands the keyboard back to the panes.
        "esc" => {
            drop(guard);
            return Some(AttachInputResult {
                consumed: true,
                release_capture: true,
                ..AttachInputResult::default()
            });
        }
        // On a header, left collapses, right expands and space toggles.
        key @ ("left" | "right" | "space") => {
            let SidebarHit::Group(workspace) = target else {
                return None;
            };
            let collapsed = companion.collapsed.contains(&workspace);
            let toggle = match key {
                "left" => !collapsed,
                "right" => collapsed,
                _ => true,
            };
            let status_message = toggle.then(|| companion.toggle_group(workspace)).flatten();
            let dirty = toggle && publish_companion(companion).is_ok();
            drop(guard);
            return Some(AttachInputResult {
                consumed: true,
                dirty,
                status_message,
                ..AttachInputResult::default()
            });
        }
        _ => return None,
    };
    let previous_scroll = companion.list.borrow().scroll;
    let previous_cursor = companion.cursor.clone();
    // The keyboard moves from the focused row; the highlight follows it.
    let keys = companion.item_keys();
    companion.cursor.set_cursor(Some(target));
    if let CursorOutcome::Moved(next) = companion.cursor.apply(&keys, input) {
        companion.cursor.set_hovered(Some(next));
        companion.reveal_item(next);
    } else {
        companion.cursor.set_hovered(Some(target));
    }
    let dirty = {
        companion.revision = companion.revision.saturating_add(1).max(1);
        if publish_companion(companion).is_ok() {
            true
        } else {
            companion.list.borrow_mut().scroll = previous_scroll;
            companion.cursor = previous_cursor;
            false
        }
    };
    drop(guard);
    Some(AttachInputResult {
        consumed: true,
        dirty,
        ..AttachInputResult::default()
    })
}

/// A left press or Enter on a tab's card switches to that tab: answered here,
/// as an `activate-tab` invocation the attach client runs, because a handled
/// endpoint never reaches the plugin's service.
fn activate_tab(owner: &CompanionHandle, event: &AttachInputEvent) -> Option<AttachInputResult> {
    if !is_activation(event) {
        return None;
    }
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    match hit(companion, event)? {
        // The tab shown takes the keyboard: activating it leaves the sidebar.
        SidebarHit::Tab(target) => {
            drop(guard);
            Some(AttachInputResult {
                consumed: true,
                preserve_focus: true,
                release_capture: true,
                service_invocation: activate_tab_invocation(target),
                ..AttachInputResult::default()
            })
        }
        // A header collapses or expands its workspace's group.
        SidebarHit::Group(workspace) => {
            let status_message = companion.toggle_group(workspace);
            let dirty = publish_companion(companion).is_ok();
            drop(guard);
            Some(AttachInputResult {
                consumed: true,
                dirty,
                status_message,
                ..AttachInputResult::default()
            })
        }
        SidebarHit::ResizeEdge | SidebarHit::Scrollbar => None,
    }
}

/// A left press, or Enter, both of which switch to the tab under them.
fn is_activation(event: &AttachInputEvent) -> bool {
    let key = event.event_kind == "key"
        && matches!(event.phase.as_str(), "press" | "repeat")
        && event.key.as_deref() == Some("enter");
    let pointer = event.event_kind == "pointer"
        && event.phase == "down"
        && event.button.as_deref() == Some("left");
    key || pointer
}

/// The workspaces plugin's `activate-tab` command for `tab`, for the client
/// to run: it shows that exact tab, switching to its workspace when it is in
/// another one.
fn activate_tab_invocation(tab: Uuid) -> Option<bmux_plugin::AttachInputServiceInvocation> {
    use bmux_workspaces_plugin_api::workspaces_activation_v1::client::{
        ActivateTabEndpoint as Endpoint, ActivateTabRequest,
    };
    let payload =
        bmux_plugin_sdk::encode_service_message(&ActivateTabRequest { context_id: tab }).ok()?;
    let payload = bmux_plugin_sdk::service_contract::encode(
        Endpoint::INTERFACE_ID.as_str(),
        Endpoint::SERVICE_CONTRACT,
        &payload,
    )
    .ok()?;
    Some(bmux_plugin::AttachInputServiceInvocation {
        endpoint: bmux_plugin::AttachInputEndpoint {
            capability: Endpoint::CAPABILITY.to_string(),
            interface_id: Endpoint::INTERFACE_ID.to_string(),
            operation: Endpoint::OPERATION.to_string(),
        },
        response_endpoint: None,
        payload,
    })
}

fn handle_local_input(
    owner: &CompanionHandle,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    // A failed allocation/model publication leaves authoritative data intact,
    // but old hooks must not navigate or activate against its unpublished geometry.
    // Retry the complete snapshot and consume this event even after repair: it
    // originated from the previous presentation.
    if let Ok(guard) = owner.lock()
        && let Some(companion) = guard.as_ref()
        && companion.presentation_pending.get()
    {
        return Some(AttachInputResult {
            consumed: true,
            dirty: publish_companion(companion).is_ok(),
            ..AttachInputResult::default()
        });
    }
    if let Some(result) = update_resize(owner, event) {
        return Some(result);
    }
    if let Some(result) = activate_tab(owner, event) {
        return Some(result);
    }
    if let Some(result) = update_keyboard(owner, event) {
        return Some(result);
    }
    if event.event_kind != "pointer" {
        return None;
    }
    if update_scroll(owner, event) {
        return Some(AttachInputResult {
            consumed: true,
            dirty: true,
            ..AttachInputResult::default()
        });
    }
    if update_hover(owner, event) {
        return Some(AttachInputResult {
            consumed: false,
            dirty: true,
            ..AttachInputResult::default()
        });
    }
    None
}

fn handle_input(context: &NativeServiceContext, event: &AttachInputEvent) -> AttachInputResult {
    if !is_activation(event) {
        return AttachInputResult::default();
    }
    let Some(target) = event
        .target
        .as_ref()
        .and_then(|target| region_tab_id(&target.region))
    else {
        return AttachInputResult::default();
    };
    let mut client = ServiceCallerDispatchClient::new(context);
    match block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_activation_v1::client::activate_tab(
            &mut client,
            target,
        ),
    ) {
        Ok(Ok(_)) => AttachInputResult {
            consumed: true,
            dirty: true,
            ..AttachInputResult::default()
        },
        Ok(Err(error)) => AttachInputResult {
            consumed: true,
            status_message: Some(format!("tab switch failed: {error:?}")),
            ..AttachInputResult::default()
        },
        Err(error) => AttachInputResult {
            consumed: true,
            status_message: Some(format!("tab switch unavailable: {error}")),
            ..AttachInputResult::default()
        },
    }
}

bmux_plugin_sdk::plugin!(SidebarPlugin, create = |_| Ok(SidebarPlugin));

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn scoped_frontends_keep_fact_geometry_and_lifetime_independent() {
        use bmux_presentation_state::{PresentationFactRegistry, PresentationFactSnapshot};
        use std::sync::Arc;

        let first_resources = bmux_plugin::AttachPresentationResources::isolated(32);
        let second_resources = bmux_plugin::AttachPresentationResources::isolated(32);
        let tab_id = Uuid::from_u128(0x100);
        let tabs = tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id: tab_id,
                name: "one".into(),
                active: true,
                workspace: "default".into(),
                workspace_id: Uuid::nil(),
            }],
            revision: 1,
        };
        for resources in [&first_resources, &second_resources] {
            let _sender = resources
                .events
                .register_state_channel::<tabs_list::TabListSnapshot>(
                    tabs_list::STATE_KIND,
                    tabs.clone(),
                );
        }
        let first_facts = Arc::new(PresentationFactRegistry::new());
        let second_facts = Arc::new(PresentationFactRegistry::new());
        let first =
            SidebarPresentation::install_scoped(None, &first_resources, first_facts.clone())
                .unwrap();
        let second =
            SidebarPresentation::install_scoped(None, &second_resources, second_facts).unwrap();
        let saved_second = second_resources.surfaces.owner_snapshot(OWNER).unwrap();
        let before = first_resources
            .surfaces
            .owner_snapshot(OWNER)
            .unwrap()
            .revision;
        first_facts
            .publish(
                "scoped-test",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![PresentationFact {
                        entity: PresentationEntityRef::new("bmux.tabs", tab_id.to_string()),
                        key: "activity".into(),
                        role: PresentationFactRole::Warning,
                        short_text: "waiting for a very long scoped operation".into(),
                        detail_text: None,
                        icon_id: None,
                        priority: 1,
                    }],
                },
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while first_resources
                .surfaces
                .owner_snapshot(OWNER)
                .unwrap()
                .revision
                == before
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            second_resources.surfaces.owner_snapshot(OWNER).unwrap(),
            saved_second
        );
        drop(first);
        assert!(first_resources.surfaces.owner_snapshot(OWNER).is_none());
        assert_eq!(
            second_resources.surfaces.owner_snapshot(OWNER).unwrap(),
            saved_second
        );
        drop(second);
    }

    use super::*;
    use bmux_plugin::RenderOp;

    /// Most tests exercise a shown sidebar; the shipped default is hidden.
    fn shown_toml() -> toml::Value {
        toml::Value::Table(toml::toml! { visible = true })
    }

    fn shown_settings() -> Settings {
        Settings {
            visibility: VisibilitySettings {
                visible: true,
                ..VisibilitySettings::default()
            },
            ..Settings::default()
        }
    }

    #[test]
    fn sidebar_starts_hidden_by_default() {
        assert!(!Settings::default().visibility.visible);
        assert!(!Settings::parse(None).unwrap().visibility.visible);
        assert!(!CompanionState::new(Settings::default(), None).visible());
    }

    #[test]
    fn republish_advances_past_retained_owner_revision() {
        let resources = bmux_plugin::AttachPresentationResources::isolated(64);
        let presentation = SidebarPresentation::install(
            None,
            resources.layouts.clone(),
            resources.allocations.clone(),
            resources.input.clone(),
            resources.surfaces.clone(),
        )
        .expect("install sidebar");
        presentation
            .publish(tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 0,
            })
            .expect("initial publish");
        resources
            .surfaces
            .publish(
                OWNER,
                PluginSurfaceSnapshot {
                    revision: 9,
                    surfaces: Vec::new(),
                },
            )
            .expect("seed higher owner revision");
        presentation
            .publish(tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 1,
            })
            .expect("republish above retained revision");
        let revision = || {
            resources
                .surfaces
                .owner_snapshot(OWNER)
                .expect("owner snapshot")
                .revision
        };
        assert_eq!(revision(), 10);
        let foreign_identity = std::sync::Arc::new(Mutex::new(None));
        publish_for_installation(
            &foreign_identity,
            tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 99,
            },
        )
        .unwrap();
        assert_eq!(revision(), 10);
        assert!(
            resources
                .layouts
                .requests()
                .iter()
                .any(|request| request.id.owner_plugin_id == OWNER)
        );
        // Dropping the presentation removes only its own runtime's entries.
        drop(presentation);
        assert!(
            !resources
                .layouts
                .requests()
                .iter()
                .any(|request| request.id.owner_plugin_id == OWNER)
        );
    }

    #[test]
    fn two_runtimes_keep_independent_sidebars() {
        let install = |resources: &bmux_plugin::AttachPresentationResources| {
            SidebarPresentation::install(
                None,
                resources.layouts.clone(),
                resources.allocations.clone(),
                resources.input.clone(),
                resources.surfaces.clone(),
            )
            .unwrap()
        };
        let (first_resources, second_resources) = (
            bmux_plugin::AttachPresentationResources::isolated(64),
            bmux_plugin::AttachPresentationResources::isolated(64),
        );
        let first = install(&first_resources);
        let second = install(&second_resources);
        first
            .publish(tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 1,
            })
            .unwrap();
        assert!(first_resources.surfaces.owner_snapshot(OWNER).is_some());
        assert!(second_resources.surfaces.owner_snapshot(OWNER).is_none());
        drop(first);
        assert!(
            second_resources
                .layouts
                .requests()
                .iter()
                .any(|request| request.id.owner_plugin_id == OWNER)
        );
        drop(second);
    }

    #[test]
    fn layout_request_places_sidebar_on_configured_edge_and_order() {
        let left = layout_request(&shown_settings(), 28, true);
        let right = layout_request(
            &Settings {
                placement: Placement::Right,
                order: 50,
                ..shown_settings()
            },
            28,
            true,
        );
        assert!(matches!(
            left.operation,
            bmux_plugin::layout::LayoutOperation::Split {
                edge: LayoutEdge::Left,
                ..
            }
        ));
        assert_eq!(right.order, 50);
        assert!(matches!(
            right.operation,
            bmux_plugin::layout::LayoutOperation::Split {
                edge: LayoutEdge::Right,
                ..
            }
        ));
    }

    fn runtime_resources(
        tabs: tabs_list::TabListSnapshot,
        local_state_dir: Option<std::path::PathBuf>,
    ) -> bmux_plugin::AttachPresentationResources {
        let mut resources = bmux_plugin::AttachPresentationResources::isolated(8);
        resources.local_state_dir = local_state_dir;
        resources
            .events
            .register_state_channel(tabs_list::STATE_KIND, tabs);
        resources
    }

    fn one_tab(id: Uuid) -> tabs_list::TabListSnapshot {
        tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id,
                name: "one".into(),
                active: true,
                workspace: "default".into(),
                workspace_id: Uuid::nil(),
            }],
            revision: 1,
        }
    }

    fn sidebar_split_visible(resources: &bmux_plugin::AttachPresentationResources) -> bool {
        let requests = resources.layouts.requests();
        assert_eq!(requests.len(), 1);
        match requests[0].operation {
            bmux_plugin::layout::LayoutOperation::Split { .. } => true,
            bmux_plugin::layout::LayoutOperation::Hidden => false,
            bmux_plugin::layout::LayoutOperation::Overlay => panic!("unexpected overlay"),
        }
    }

    fn sidebar_surface_count(resources: &bmux_plugin::AttachPresentationResources) -> usize {
        resources
            .surfaces
            .owner_snapshot(OWNER)
            .map_or(0, |snapshot| snapshot.surfaces.len())
    }

    #[tokio::test]
    async fn runtime_install_reads_facts_from_the_runtime_fact_host() {
        let tab_id = Uuid::from_u128(0x200);
        let resources = runtime_resources(one_tab(tab_id), None);
        let presentation = SidebarPresentation::install_for_runtime(None, &resources).unwrap();
        let host = resources
            .states
            .get_or_insert_with(bmux_presentation_state::PresentationFactHostService::new);
        host.read()
            .unwrap()
            .registry()
            .publish(
                "runtime-test",
                bmux_presentation_state::PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![PresentationFact {
                        entity: PresentationEntityRef::new("bmux.tabs", tab_id.to_string()),
                        key: "activity".into(),
                        role: PresentationFactRole::Warning,
                        short_text: "building".into(),
                        detail_text: None,
                        icon_id: None,
                        priority: 1,
                    }],
                },
            )
            .unwrap();
        let guard = presentation.owner.lock().unwrap();
        let companion = guard.as_ref().unwrap();
        let tab = companion.snapshot.tabs[0].clone();
        let fact = companion.tab_fact(&tab);
        drop(guard);
        assert_eq!(
            fact.as_ref().map(|fact| fact.short_text.as_str()),
            Some("building")
        );
        assert_eq!(
            render_template(concat!("{", "fact}"), &tab, 0, false, fact.as_ref(), None),
            "building"
        );
    }

    #[tokio::test]
    async fn visibility_commands_hide_and_show_only_this_runtime() {
        let tab_id = Uuid::from_u128(0x300);
        let first = runtime_resources(one_tab(tab_id), None);
        let second = runtime_resources(one_tab(tab_id), None);
        let _first = SidebarPresentation::install_for_runtime(Some(&shown_toml()), &first).unwrap();
        let _second =
            SidebarPresentation::install_for_runtime(Some(&shown_toml()), &second).unwrap();
        assert!(sidebar_split_visible(&first));
        assert_eq!(sidebar_surface_count(&first), 1);

        let hidden = first.input.command(OWNER, "hide", &[]).unwrap();
        assert!(hidden.consumed && hidden.dirty);
        assert!(!sidebar_split_visible(&first));
        assert_eq!(sidebar_surface_count(&first), 0);
        assert!(sidebar_split_visible(&second));
        assert_eq!(sidebar_surface_count(&second), 1);

        // Hiding again changes nothing.
        assert!(!first.input.command(OWNER, "hide", &[]).unwrap().dirty);

        let shown = first.input.command(OWNER, "toggle", &[]).unwrap();
        assert!(shown.dirty);
        assert!(sidebar_split_visible(&first));
        assert_eq!(sidebar_surface_count(&first), 1);
    }

    #[tokio::test]
    async fn hidden_sidebar_ignores_its_empty_allocation() {
        let resources = runtime_resources(one_tab(Uuid::from_u128(0x400)), None);
        let presentation =
            SidebarPresentation::install_for_runtime(Some(&shown_toml()), &resources).unwrap();
        let id = PluginLayoutId::new(OWNER, LAYOUT_ID);
        let shown = ExtensionRect::new(0, 0, 28, 20);
        resources.allocations.notify(&id, shown);
        resources.input.command(OWNER, "hide", &[]).unwrap();
        resources
            .allocations
            .notify(&id, ExtensionRect::new(28, 0, 0, 0));
        assert_eq!(sidebar_surface_count(&resources), 0);
        assert_eq!(
            presentation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .allocation,
            Some(shown)
        );
        resources.input.command(OWNER, "show", &[]).unwrap();
        assert_eq!(sidebar_surface_count(&resources), 1);
    }

    #[tokio::test]
    async fn remembered_visibility_restores_on_the_next_install() {
        let directory =
            std::env::temp_dir().join(format!("bmux-sidebar-visibility-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let remember = toml::Value::Table(toml::toml! { visible = true
        remember_visibility = true });
        let resources = runtime_resources(one_tab(Uuid::from_u128(0x500)), Some(directory.clone()));
        let presentation =
            SidebarPresentation::install_for_runtime(Some(&remember), &resources).unwrap();
        resources.input.command(OWNER, "hide", &[]).unwrap();
        drop(presentation);
        assert!(resources.input.command(OWNER, "show", &[]).is_none());

        let restored = runtime_resources(one_tab(Uuid::from_u128(0x500)), Some(directory.clone()));
        let _remembering =
            SidebarPresentation::install_for_runtime(Some(&remember), &restored).unwrap();
        assert!(!sidebar_split_visible(&restored));
        assert_eq!(sidebar_surface_count(&restored), 0);

        // Remembering is the default.
        let by_default =
            runtime_resources(one_tab(Uuid::from_u128(0x500)), Some(directory.clone()));
        let _by_default =
            SidebarPresentation::install_for_runtime(Some(&shown_toml()), &by_default).unwrap();
        assert!(!sidebar_split_visible(&by_default));

        // Turned off, the configured default applies.
        let forget = toml::Value::Table(toml::toml! { visible = true
        remember_visibility = false });
        let forgetful = runtime_resources(one_tab(Uuid::from_u128(0x500)), Some(directory.clone()));
        let _forgetful =
            SidebarPresentation::install_for_runtime(Some(&forget), &forgetful).unwrap();
        assert!(sidebar_split_visible(&forgetful));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn configured_hidden_sidebar_starts_hidden_and_rejects_non_boolean_settings() {
        let hidden = toml::Value::Table(toml::toml! { visible = false });
        assert!(!Settings::parse(Some(&hidden)).unwrap().visibility.visible);
        let invalid = toml::Value::Table(toml::toml! { remember_visibility = "yes" });
        assert!(Settings::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn settings_validate_width_bounds() {
        let settings: toml::Value = toml::from_str(
            "placement = 'right'\nwidth = 24\nminimum_width = 12\nmaximum_width = 40",
        )
        .unwrap();
        let settings = Settings::parse(Some(&settings)).unwrap();
        assert_eq!(settings.placement, Placement::Right);
        assert_eq!(settings.title_template, "{state_icon} {name}");
        assert_eq!(settings.collapse_below_width, 80);
        let invalid: toml::Value =
            toml::from_str("width = 10\nminimum_width = 12\nmaximum_width = 40").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn templates_escape_braces_and_expand_stable_tab_fields() {
        let tab = tabs_list::TabListEntry {
            id: Uuid::from_u128(10),
            name: "build".to_string(),
            active: true,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        assert_eq!(
            render_template(
                "{{literal}} {index}{name} {active}",
                &tab,
                1,
                true,
                None,
                None
            ),
            "{literal} 2 build active"
        );
    }

    #[test]
    fn semantic_fact_populates_templates_and_style_role() {
        let tab = tabs_list::TabListEntry {
            id: Uuid::from_u128(20),
            name: "build".to_string(),
            active: false,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        let fact = PresentationFact {
            entity: PresentationEntityRef::new("bmux.tabs", tab.id.to_string()),
            key: "activity".to_string(),
            role: PresentationFactRole::Warning,
            short_text: "waiting".to_string(),
            detail_text: Some("approval required".to_string()),
            icon_id: Some("attention".to_string()),
            priority: 5,
        };
        assert_eq!(
            render_template(
                concat!("{name} ", "{", "fact}", " {fact_detail} {fact_icon}"),
                &tab,
                0,
                true,
                Some(&fact),
                None,
            ),
            "build waiting approval required attention"
        );
        assert_eq!(
            fact_style(fact.role, RenderStyle::new()).fg,
            Some(bmux_plugin::RenderColor::Named(
                RenderNamedColor::BrightYellow
            ))
        );
    }

    #[test]
    fn hover_style_changes_only_the_target_card_title() {
        let mut state = CompanionState::new(shown_settings(), None);
        let first = Uuid::from_u128(11);
        let second = Uuid::from_u128(12);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![
                tabs_list::TabListEntry {
                    id: first,
                    name: "one".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
                tabs_list::TabListEntry {
                    id: second,
                    name: "two".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
            ],
            revision: 1,
        });
        let before = build_surface(&state, 1);
        state.cursor.set_hovered(Some(SidebarHit::Tab(second)));
        let after = build_surface(&state, 2);
        // The first card's row is untouched; the hovered card's is restyled.
        assert_eq!(painted_row(&before, 1), painted_row(&after, 1));
        assert_ne!(painted_row(&before, 2), painted_row(&after, 2));
        assert_eq!(surface_text(&before, 32, 3), surface_text(&after, 32, 3));
    }

    #[tokio::test]
    async fn presentation_subscription_uses_local_bus_and_stops_on_drop() {
        let resources = bmux_plugin::AttachPresentationResources::isolated(4);
        let snapshot = |revision| tabs_list::TabListSnapshot {
            tabs: Vec::new(),
            revision,
        };
        resources
            .events
            .register_state_channel(tabs_list::STATE_KIND, snapshot(1));
        let presentation = SidebarPresentation::install_for_runtime(None, &resources).unwrap();
        let revision = |presentation: &SidebarPresentation| {
            presentation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .snapshot
                .revision
        };
        assert_eq!(revision(&presentation), 1);
        resources
            .events
            .publish_state(&tabs_list::STATE_KIND, snapshot(2))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while revision(&presentation) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let owner = presentation.owner.clone();
        drop(presentation);
        resources
            .events
            .publish_state(&tabs_list::STATE_KIND, snapshot(3))
            .unwrap();
        tokio::task::yield_now().await;
        assert!(owner.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn active_tab_follows_this_attachments_selection_not_the_catalog_flag() {
        let resources = bmux_plugin::AttachPresentationResources::isolated(4);
        let first = Uuid::from_u128(21);
        let second = Uuid::from_u128(22);
        let entry = |id, active| tabs_list::TabListEntry {
            id,
            name: id.to_string(),
            active,
            workspace: "default".to_string(),
            workspace_id: Uuid::nil(),
        };
        // The shared catalog claims `first`; this attachment selected `second`.
        resources.events.register_state_channel(
            tabs_list::STATE_KIND,
            tabs_list::TabListSnapshot {
                tabs: vec![entry(first, true), entry(second, false)],
                revision: 1,
            },
        );
        resources.events.register_state_channel(
            bmux_tabs_plugin_api::tabs_local_view::STATE_KIND,
            bmux_tabs_plugin_api::tabs_local_view::TabSelection {
                context_id: Some(second),
            },
        );
        let presentation = SidebarPresentation::install_for_runtime(None, &resources).unwrap();
        let active = |presentation: &SidebarPresentation| {
            presentation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .snapshot
                .tabs
                .iter()
                .filter(|tab| tab.active)
                .map(|tab| tab.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(active(&presentation), vec![second]);

        resources
            .events
            .publish_state(
                &bmux_tabs_plugin_api::tabs_local_view::STATE_KIND,
                bmux_tabs_plugin_api::tabs_local_view::TabSelection {
                    context_id: Some(first),
                },
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while active(&presentation) != vec![first] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // A later catalog without any flag keeps this attachment's selection.
        resources
            .events
            .publish_state(
                &tabs_list::STATE_KIND,
                tabs_list::TabListSnapshot {
                    tabs: vec![entry(first, false), entry(second, false)],
                    revision: 2,
                },
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let revision = presentation
                    .owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .revision;
                if revision == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(active(&presentation), vec![first]);
    }

    #[test]
    fn independent_installations_route_allocations_and_release_registries() {
        let create = || {
            let layouts = std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4));
            let allocations =
                std::sync::Arc::new(bmux_plugin::layout::AllocationRegistry::default());
            let input = std::sync::Arc::new(bmux_plugin::AttachPresentationInputRegistry::new());
            let surfaces = std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4));
            let presentation = SidebarPresentation::install(
                Some(&shown_toml()),
                layouts.clone(),
                allocations.clone(),
                input,
                surfaces.clone(),
            )
            .unwrap();
            (presentation, layouts, allocations, surfaces)
        };
        let (first, first_layouts, first_allocations, first_surfaces) = create();
        let (second, second_layouts, second_allocations, second_surfaces) = create();
        let id = PluginLayoutId::new(OWNER, LAYOUT_ID);
        first_allocations.notify(&id, ExtensionRect::new(0, 0, 12, 5));
        second_allocations.notify(&id, ExtensionRect::new(0, 0, 30, 10));
        let saved = second_surfaces.owner_snapshot(OWNER).unwrap();
        assert_ne!(
            first_surfaces.owner_snapshot(OWNER).unwrap().surfaces,
            saved.surfaces
        );
        drop(first);
        first_allocations.notify(&id, ExtensionRect::new(0, 0, 40, 10));
        assert!(first_surfaces.owner_snapshot(OWNER).is_none());
        assert_eq!(
            first_layouts.requests(),
            [] as [bmux_plugin::layout::PluginLayoutRequest; 0]
        );
        assert_eq!(
            second_surfaces.owner_snapshot(OWNER).unwrap().surfaces,
            saved.surfaces
        );
        assert_eq!(second_layouts.requests().len(), 1);
        drop(second);
        assert!(second_surfaces.owner_snapshot(OWNER).is_none());
    }

    #[test]
    fn owned_presentations_publish_and_teardown_independently() {
        let first_registry =
            std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4));
        let second_registry =
            std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4));
        let mut first = CompanionState::new(shown_settings(), None);
        first.surfaces = Some(first_registry.clone());
        let mut second = CompanionState::new(shown_settings(), None);
        second.surfaces = Some(second_registry.clone());
        first
            .publish_allocation(ExtensionRect::new(0, 0, 12, 5), publish_companion)
            .unwrap();
        second
            .publish_allocation(ExtensionRect::new(0, 0, 30, 10), publish_companion)
            .unwrap();
        let saved = second_registry.owner_snapshot(OWNER).unwrap();
        assert_ne!(
            first_registry.owner_snapshot(OWNER).unwrap().surfaces,
            saved.surfaces
        );
        first
            .publish_allocation(ExtensionRect::new(0, 0, 20, 5), publish_companion)
            .unwrap();
        assert_eq!(
            second_registry.owner_snapshot(OWNER).unwrap().surfaces,
            saved.surfaces
        );
        drop(first);
        assert!(first_registry.owner_snapshot(OWNER).is_none());
        assert!(second_registry.owner_snapshot(OWNER).is_some());
        drop(second);
        assert!(second_registry.owner_snapshot(OWNER).is_none());
    }

    #[test]
    fn dropping_companion_releases_idle_subscription() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_sender, mut receiver) = tokio::sync::watch::channel(());
            let retained = std::sync::Arc::new(());
            let owned = retained.clone();
            let mut companion = CompanionState::new(shown_settings(), None);
            companion.subscription = Some(tokio::spawn(async move {
                let _ = receiver.changed().await;
                drop(owned);
            }));
            tokio::task::yield_now().await;
            assert_eq!(std::sync::Arc::strong_count(&retained), 2);
            drop(companion);
            tokio::task::yield_now().await;
            assert_eq!(std::sync::Arc::strong_count(&retained), 1);
        });
    }

    #[test]
    fn allocation_publication_retries_without_reapplying_resize() {
        let mut state = CompanionState::new(shown_settings(), None);
        let rect = ExtensionRect::new(0, 0, 12, 5);
        assert!(
            state
                .publish_allocation(rect, |_| Err("rejected".to_string()))
                .is_err()
        );
        assert!(state.allocation_publication_pending);
        state.list.borrow_mut().scroll.set_vertical_offset(1);
        state
            .publish_allocation(rect, |current| {
                assert_eq!(current.scroll_offset(), 1);
                Ok(())
            })
            .unwrap();
        assert!(!state.allocation_publication_pending);
        state
            .publish_allocation(rect, |_| {
                panic!("unchanged successful allocation republished")
            })
            .unwrap();
    }

    #[test]
    fn unpublished_snapshot_consumes_input_until_repaired() {
        let mut state = CompanionState::new(shown_settings(), None);
        state.surfaces = Some(std::sync::Arc::new(
            bmux_plugin::surface::PluginSurfaceRegistry::new(0),
        ));
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
        let snapshot = tabs_list::TabListSnapshot {
            tabs: vec![],
            revision: 42,
        };
        assert!(super::publish_for_installation(&owner, snapshot).is_err());
        let event = AttachInputEvent {
            target: None,
            hook_id: format!("bmux.sidebar:sidebar:tab:{}", Uuid::from_u128(1)),
            event_kind: "key".to_string(),
            phase: "press".to_string(),
            key: Some("enter".to_string()),
            button: None,
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: bmux_plugin::render::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
            screen_col: None,
            screen_row: None,
        };
        let rejected = super::handle_local_input(&owner, &event).unwrap();
        assert!(rejected.consumed);
        assert!(!rejected.dirty);
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert_eq!(state.snapshot.revision, 42);
            assert!(state.presentation_pending.get());
            state.surfaces = Some(std::sync::Arc::new(
                bmux_plugin::surface::PluginSurfaceRegistry::new(1),
            ));
            drop(guard);
        }
        let repaired = super::handle_local_input(&owner, &event).unwrap();
        assert!(repaired.consumed);
        assert!(repaired.dirty);
        assert!(
            !owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .presentation_pending
                .get()
        );
        // A subsequent allocation failure uses the same input suspension path
        // while retaining the host's new allocation and authoritative snapshot.
        let rect = ExtensionRect::new(0, 0, 9, 3);
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert!(
                state
                    .publish_allocation(rect, |_| Err("rejected".to_string()))
                    .is_err()
            );
            assert_eq!(state.allocation, Some(rect));
            assert_eq!(state.snapshot.revision, 42);
            drop(guard);
        }
        let repaired = super::handle_local_input(&owner, &event).unwrap();
        assert!(repaired.consumed && repaired.dirty);
        assert!(
            !owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .presentation_pending
                .get()
        );
    }

    /// Address `tab` through a hit table recorded for a test revision, as the
    /// host would after a successful publication.
    fn test_target(state: &CompanionState, tab: Uuid) -> bmux_plugin::AttachInputTarget {
        let mut table = HitTable::new();
        let key = table.key(SidebarHit::Tab(tab));
        state.hits.borrow_mut().publish(u64::MAX, table);
        bmux_plugin::AttachInputTarget {
            surface: SURFACE_ID.to_string(),
            region: tab_region_id(tab),
            revision: u64::MAX,
            key: Some(key),
        }
    }

    /// Two workspaces' tabs: `bmux` (tabs 1–3, tab 1 shown) with agents
    /// working, needing input and stopped, and `notes` (tab 4) idle.
    fn grouped_state(settings: Settings, state_dir: Option<std::path::PathBuf>) -> CompanionState {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, TabAgent, TabStatus, TabStatusSnapshot,
        };
        let mut state = CompanionState::new(settings, state_dir);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 14));
        let now = 600_000;
        let status = |index: u128, activity: Option<AgentActivity>| TabStatus {
            tab_id: Uuid::from_u128(index),
            panes: 1,
            running_command: None,
            cwd: None,
            title: None,
            git: None,
            last_exit: None,
            output_at_ms: None,
            attention_at_ms: None,
            attention: None,
            agent: activity.map(|activity| TabAgent {
                name: "claude".into(),
                activity,
                message: None,
                changed_at_ms: now - 5_000,
                finished_at_ms: None,
            }),
        };
        state.activity.replace(TabStatusSnapshot {
            tabs: vec![
                status(1, Some(AgentActivity::Working)),
                status(2, Some(AgentActivity::Blocked)),
                status(3, Some(AgentActivity::Idle)),
                status(4, None),
            ],
            now_ms: now,
            revision: 1,
        });
        let tab = |index: u128, name: &str, workspace: &str, workspace_id: u128| {
            tabs_list::TabListEntry {
                id: Uuid::from_u128(index),
                name: name.to_string(),
                active: index == 1,
                workspace: workspace.to_string(),
                workspace_id: Uuid::from_u128(workspace_id),
            }
        };
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![
                tab(1, "build", "bmux", 100),
                tab(2, "review", "bmux", 100),
                tab(3, "docs", "bmux", 100),
                tab(4, "todo", "notes", 200),
            ],
            revision: 1,
        });
        state
    }

    fn grouped() -> Settings {
        Settings {
            grouping: Grouping::Workspace,
            ..shown_settings()
        }
    }

    /// A pointer press on `region`, addressed by its owner-local id.
    fn press(region: &str) -> AttachInputEvent {
        AttachInputEvent {
            target: Some(bmux_plugin::AttachInputTarget {
                surface: SURFACE_ID.to_string(),
                region: region.to_string(),
                revision: 0,
                key: None,
            }),
            event_kind: "pointer".to_string(),
            phase: "down".to_string(),
            button: Some("left".to_string()),
            ..AttachInputEvent::default()
        }
    }

    fn key(region: &str, key: &str) -> AttachInputEvent {
        AttachInputEvent {
            event_kind: "key".to_string(),
            phase: "press".to_string(),
            button: None,
            key: Some(key.to_string()),
            ..press(region)
        }
    }

    /// Grouped, each workspace's tabs sit under its header, which says what
    /// they are doing: how many need input, are working and have stopped.
    #[test]
    fn grouped_tabs_sit_under_headers_that_sum_up_their_work() {
        let state = grouped_state(grouped(), None);
        let text = surface_text(&build_surface(&state, 1), 32, 8);
        assert_eq!(
            text,
            [
                " TABS                         4",
                "  ▾ bmux               ◆1 ◉1 ■1",
                "▎ ◉ build               working",
                "  ◆ review          needs input",
                "  ■ docs                stopped",
                "    5s",
                "  ▾ notes                     1",
                "  ○ todo",
            ],
            "{}",
            text.join("\n")
        );
        // Flat is unchanged: no headers.
        let flat = grouped_state(shown_settings(), None);
        let text = surface_text(&build_surface(&flat, 1), 32, 6);
        assert_eq!(text[1], "▎ ◉ build               working");
        assert_eq!(text[5], "  ○ todo");
    }

    /// Clicking a header collapses its group, and the next start remembers
    /// it; a collapsed group still sums up its work, and is marked when it
    /// hides the tab this client shows.
    #[test]
    fn clicking_a_header_collapses_its_group_and_it_is_remembered() {
        let dir = scratch_dir();
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(grouped_state(
            grouped(),
            Some(dir.clone()),
        ))));
        let bmux = format!("group:{}", Uuid::from_u128(100));
        let result = super::handle_local_input(&owner, &press(&bmux)).unwrap();
        assert!(result.consumed && result.service_invocation.is_none());
        let text = {
            let guard = owner.lock().unwrap();
            surface_text(&build_surface(guard.as_ref().unwrap(), 1), 32, 4)
        };
        assert_eq!(
            text,
            [
                " TABS                         4",
                "▎ ▸ bmux               ◆1 ◉1 ■1",
                "  ▾ notes                     1",
                "  ○ todo",
            ],
            "{}",
            text.join("\n")
        );
        drop(owner);
        let again = grouped_state(grouped(), Some(dir.clone()));
        assert_eq!(surface_text(&build_surface(&again, 1), 32, 2)[1], text[1]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// On a header, left collapses, right expands and space toggles; Enter
    /// toggles like a click.
    #[test]
    fn header_keys_collapse_and_expand_its_group() {
        let owner =
            std::sync::Arc::new(std::sync::Mutex::new(Some(grouped_state(grouped(), None))));
        let notes = Uuid::from_u128(200);
        let region = format!("group:{notes}");
        let collapsed = |owner: &CompanionHandle| {
            owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .collapsed
                .contains(&notes)
        };
        for (pressed, expected) in [
            ("left", true),
            ("left", true),
            ("right", false),
            ("right", false),
            ("space", true),
            ("enter", false),
        ] {
            super::handle_local_input(&owner, &key(&region, pressed)).unwrap();
            assert_eq!(collapsed(&owner), expected, "after {pressed}");
        }
    }

    /// Showing only the current workspace lists just the shown tab's
    /// workspace, grouped or not, and follows the shown tab.
    #[test]
    fn current_workspace_scope_lists_only_the_shown_tabs_workspace() {
        for grouping in [Grouping::None, Grouping::Workspace] {
            let mut state = grouped_state(
                Settings {
                    grouping,
                    scope: Scope::CurrentWorkspace,
                    ..shown_settings()
                },
                None,
            );
            let names = |state: &CompanionState| {
                surface_text(&build_surface(state, 1), 32, 8)
                    .iter()
                    .filter_map(|line| {
                        ["build", "review", "docs", "todo"]
                            .into_iter()
                            .find(|name| line.contains(name))
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(names(&state), ["build", "review", "docs"], "{grouping:?}");
            assert_eq!(
                surface_text(&build_surface(&state, 1), 32, 1)[0],
                " TABS                         3"
            );
            state.select(Some(Uuid::from_u128(4)));
            assert_eq!(names(&state), ["todo"], "{grouping:?}");
        }
    }

    /// A narrow header keeps the most pressing counts.
    #[test]
    fn a_narrow_header_drops_the_least_pressing_counts_first() {
        let mut state = grouped_state(grouped(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 16, 14));
        let header = surface_text(&build_surface(&state, 1), 16, 2)[1].clone();
        assert!(header.ends_with("◆1 ◉1"), "{header:?}");
        assert!(header.contains("bmux"), "{header:?}");
    }

    /// Six working agents, each a two-row card (title and directory), in a
    /// sidebar with room for six rows of them.
    fn working_cards() -> CompanionState {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, TabAgent, TabStatus, TabStatusSnapshot,
        };
        let mut state = CompanionState::new(shown_settings(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 8));
        let now = 600_000;
        state.activity.replace(TabStatusSnapshot {
            tabs: (1..=6_u128)
                .map(|index| TabStatus {
                    tab_id: Uuid::from_u128(index),
                    panes: 1,
                    running_command: Some("claude".into()),
                    cwd: Some(format!("/work/repo-{index}")),
                    title: None,
                    git: None,
                    last_exit: None,
                    output_at_ms: None,
                    attention_at_ms: None,
                    attention: None,
                    agent: Some(TabAgent {
                        name: "claude".into(),
                        activity: AgentActivity::Working,
                        message: None,
                        changed_at_ms: now - 1_000,
                        finished_at_ms: None,
                    }),
                })
                .collect(),
            now_ms: now,
            revision: 1,
        });
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (1..=6_u128)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        state
    }

    /// Scrolled to any offset, even partway into a card, every card whose
    /// title is on screen shows its state glyph in its state's color and its
    /// summary at the right of that title row.
    #[test]
    fn scrolled_cards_keep_their_state_glyph_and_summary() {
        let green = Some(RenderColor::Named(RenderNamedColor::Green));
        for offset in 0..=6_i64 {
            let state = working_cards();
            state.scroll_rows(offset);
            assert_eq!(state.scroll_offset(), u64::try_from(offset).unwrap());
            let surface = build_surface(&state, 1);
            let text = surface_text(&surface, 32, 8);
            let titles = (1..8_u16)
                .filter(|row| text[usize::from(*row)].contains("tab-"))
                .collect::<Vec<_>>();
            assert!(titles.len() >= 2, "offset {offset}: {text:?}");
            for row in titles {
                let line = &text[usize::from(row)];
                // The overflowing list's scrollbar sits after the summary.
                let content = line.trim_end_matches(['▐', '▕']).trim_end();
                assert!(
                    content.ends_with("working"),
                    "offset {offset}, row {row}: {line:?}"
                );
                let runs = painted_row(&surface, row);
                let glyph = runs
                    .iter()
                    .find(|(x, text, _)| *x == 2 && text.starts_with('◉'));
                assert_eq!(
                    glyph.map(|(_, _, style)| style.fg),
                    Some(green),
                    "offset {offset}, row {row}: {runs:?}"
                );
                let summary = runs.iter().find(|(_, text, _)| text.contains("working"));
                assert_eq!(
                    summary.map(|(_, _, style)| style.fg),
                    Some(green),
                    "offset {offset}, row {row}: {runs:?}"
                );
            }
        }
    }

    /// After scrolling, each card's hit region covers exactly the rows its
    /// card paints, inside the list: none overlap, none are off screen, and
    /// every card with a visible title can be clicked there.
    #[test]
    fn hit_regions_match_painted_cards_after_scrolling() {
        for offset in 0..=6_i64 {
            let state = working_cards();
            state.scroll_rows(offset);
            let surface = build_surface(&state, 1);
            let text = surface_text(&surface, 32, 8);
            let mut covered = std::collections::BTreeSet::new();
            for region in tab_regions(&surface) {
                let id = region_tab_id(&region.local_id).unwrap();
                let name = format!("tab-{}", id.as_u128());
                assert!(
                    region.rect.y >= 1 && region.rect.y + region.rect.h <= 7,
                    "{region:?}"
                );
                for row in region.rect.y..region.rect.y + region.rect.h {
                    assert!(covered.insert(row), "offset {offset}: row {row} twice");
                    let line = &text[usize::from(row)];
                    let card_row =
                        line.contains(&name) || line.contains(&format!("repo-{}", id.as_u128()));
                    assert!(card_row, "offset {offset}: {name} region on {line:?}");
                }
            }
            for (row, line) in text.iter().enumerate().skip(1) {
                if let Some(start) = line.find("tab-") {
                    let name = line[start..].split_whitespace().next().unwrap();
                    let row = u16::try_from(row).unwrap();
                    assert!(
                        tab_regions(&surface).iter().any(|region| {
                            region_tab_id(&region.local_id)
                                .is_some_and(|id| format!("tab-{}", id.as_u128()) == name)
                                && (region.rect.y..region.rect.y + region.rect.h).contains(&row)
                        }),
                        "offset {offset}: no region for {name} on row {row}"
                    );
                }
            }
        }
    }

    /// Clicking, or pressing Enter on, a tab in another workspace shows that
    /// tab: the click asks the workspaces plugin to activate it.
    #[test]
    fn clicking_a_tab_in_another_workspace_activates_it() {
        let owner =
            std::sync::Arc::new(std::sync::Mutex::new(Some(grouped_state(grouped(), None))));
        let other = Uuid::from_u128(4);
        let region = tab_region_id(other);
        for event in [press(&region), key(&region, "enter")] {
            let result = super::handle_local_input(&owner, &event).unwrap();
            assert!(result.consumed);
            // The tab shown takes the keyboard, not the sidebar.
            assert!(result.preserve_focus && result.release_capture);
            assert_activates(&result.service_invocation.unwrap(), other);
        }
    }

    /// While the sidebar has the keyboard its heading lights up and the
    /// focused row is highlighted; Escape hands the keyboard back.
    #[test]
    fn the_sidebar_shows_when_it_has_the_keyboard() {
        let owner =
            std::sync::Arc::new(std::sync::Mutex::new(Some(grouped_state(grouped(), None))));
        let heading_style = |owner: &CompanionHandle| {
            let guard = owner.lock().unwrap();
            let surface = build_surface(guard.as_ref().unwrap(), 1);
            drop(guard);
            painted_row(&surface, 0)
                .into_iter()
                .find(|(_, text, _)| text.contains("TABS"))
                .map(|(_, _, style)| style)
                .unwrap()
        };
        let unfocused = heading_style(&owner);
        let review = Uuid::from_u128(2);
        let hook = format!("{OWNER}:{SURFACE_ID}:{}", tab_region_id(review));
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert!(state.set_focus(&hook, true));
            assert_eq!(state.cursor.hovered(), Some(&SidebarHit::Tab(review)));
            drop(guard);
        }
        let focused = heading_style(&owner);
        assert_ne!(focused, unfocused);
        assert!(focused.underline);
        // Losing focus puts it back.
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert!(state.set_focus(&hook, false));
            assert_eq!(state.cursor.hovered(), None);
            assert!(!state.set_focus(&hook, false));
            drop(guard);
        }
        assert_eq!(heading_style(&owner), unfocused);
        let escape =
            super::handle_local_input(&owner, &key(&tab_region_id(review), "esc")).unwrap();
        assert!(escape.consumed && escape.release_capture);
    }

    /// Rows take the keyboard under the pointer where focus-on-hover is on;
    /// the resize edge and the scrollbar never do.
    #[test]
    fn rows_take_focus_by_hover_and_the_edge_and_scrollbar_do_not() {
        let state = working_cards();
        let surface = build_surface(&state, 1);
        let hover = |id: &str| {
            surface
                .interactive_regions
                .iter()
                .find(|region| region.local_id == id)
                .map(|region| region.focus_on_hover)
        };
        assert_eq!(hover(&tab_region_id(Uuid::from_u128(1))), Some(true));
        assert_eq!(hover(RESIZE_EDGE_REGION), Some(false));
        assert_eq!(hover(view::SCROLLBAR), Some(false));
    }

    /// The scrollbar is a slim bar against the panes while the sidebar is not
    /// in use, and the full-width thumb while it has the keyboard or the
    /// pointer is on its column.
    #[test]
    fn the_scrollbar_is_slim_until_the_sidebar_is_in_use() {
        let mut state = working_cards();
        let thumb = build_surface(&state, 1)
            .interactive_regions
            .into_iter()
            .find(|region| region.local_id == view::SCROLLBAR)
            .expect("a thumb while the rows overflow")
            .rect;
        let thumb_glyph = |state: &CompanionState| {
            painted_row(&build_surface(state, 2), thumb.y)
                .into_iter()
                .find(|(x, _, _)| *x == thumb.x)
                .map(|(_, text, _)| text)
        };
        assert_eq!(thumb_glyph(&state).as_deref(), Some("▐"));
        state.engaged.focused = true;
        assert_eq!(thumb_glyph(&state).as_deref(), Some("█"));
        state.engaged.focused = false;
        state.engaged.edge_hovered = true;
        assert_eq!(thumb_glyph(&state).as_deref(), Some("█"));
    }

    /// Rows that overflow put a scrollbar in the resize edge's own column:
    /// one column faces the panes. Grabbing the thumb scrolls from where it
    /// was grabbed, a click on the track jumps there, and pressing the track
    /// and moving sideways resizes; rows that fit show only the edge.
    #[test]
    fn the_edge_column_carries_the_scrollbar_and_its_gestures() {
        let state = working_cards();
        let surface = build_surface(&state, 1);
        let region = |surface: &PluginSurface, id: &str| {
            surface
                .interactive_regions
                .iter()
                .find(|region| region.local_id == id)
                .map(|region| region.rect)
        };
        let thumb = region(&surface, view::SCROLLBAR).expect("a thumb while the rows overflow");
        let edge = region(&surface, RESIZE_EDGE_REGION).unwrap();
        assert_eq!((thumb.x, edge.x), (31, 31), "one column for both");
        assert_eq!(
            tab_regions(&surface)[0].rect.w,
            31,
            "the list gives up only that column"
        );

        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
        let pointer = |region: &str, phase: &str, col: u16, row: u16| AttachInputEvent {
            screen_col: Some(col),
            screen_row: Some(row),
            ..AttachInputEvent {
                phase: phase.to_string(),
                ..press(region)
            }
        };
        let offset =
            |owner: &CompanionHandle| owner.lock().unwrap().as_ref().unwrap().scroll_offset();
        let width = |owner: &CompanionHandle| owner.lock().unwrap().as_ref().unwrap().width;
        // The thumb shows a hand that grabs it, closed while it scrolls.
        let thumb_cursor = |owner: &CompanionHandle| {
            let guard = owner.lock().unwrap();
            build_surface(guard.as_ref().unwrap(), 9)
                .interactive_regions
                .into_iter()
                .find(|region| region.local_id == view::SCROLLBAR)
                .map(|region| region.cursor)
        };
        let hand = |shape| {
            Some(bmux_plugin::surface::PluginSurfaceCursor::Tui {
                intent: bmux_tui::pointer::PointerIntent::Shape(shape),
                role: bmux_tui::hit::HitRole::DragHandle,
            })
        };
        assert_eq!(
            thumb_cursor(&owner),
            hand(bmux_tui::pointer::PointerShape::Grab)
        );
        // Grab the thumb at its top and drag it to the bottom of the track.
        let grabbed =
            super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "down", 31, thumb.y))
                .unwrap();
        assert!(grabbed.capture_pointer && grabbed.preserve_focus);
        super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "drag", 31, 6)).unwrap();
        let bottom = offset(&owner);
        assert!(bottom > 0, "dragging the thumb down scrolls down");
        assert_eq!(
            thumb_cursor(&owner),
            hand(bmux_tui::pointer::PointerShape::Grabbing)
        );
        super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "up", 31, 6)).unwrap();
        assert_eq!(
            thumb_cursor(&owner),
            hand(bmux_tui::pointer::PointerShape::Grab)
        );
        // A click on the track near the top jumps there, and resizes nothing.
        let before = width(&owner);
        super::handle_local_input(&owner, &pointer(RESIZE_EDGE_REGION, "down", 31, 1)).unwrap();
        let clicked =
            super::handle_local_input(&owner, &pointer(RESIZE_EDGE_REGION, "up", 31, 1)).unwrap();
        assert!(clicked.release_capture);
        assert!(
            offset(&owner) < bottom,
            "a click on the track jumps toward it"
        );
        assert_eq!(width(&owner), before);
        // Pressing the track and moving sideways resizes, and scrolls nothing.
        let scrolled = offset(&owner);
        super::handle_local_input(&owner, &pointer(RESIZE_EDGE_REGION, "down", 31, 6)).unwrap();
        super::handle_local_input(&owner, &pointer(RESIZE_EDGE_REGION, "drag", 35, 6)).unwrap();
        super::handle_local_input(&owner, &pointer(RESIZE_EDGE_REGION, "up", 35, 6)).unwrap();
        assert_eq!(width(&owner), before + 4);
        assert_eq!(offset(&owner), scrolled);

        // Sideways from the thumb resizes too: the first movement decides.
        let thumb = {
            let guard = owner.lock().unwrap();
            let surface = build_surface(guard.as_ref().unwrap(), 2);
            drop(guard);
            region(&surface, view::SCROLLBAR).unwrap()
        };
        let scrolled = offset(&owner);
        let wider = width(&owner);
        super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "down", 35, thumb.y)).unwrap();
        super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "drag", 37, thumb.y)).unwrap();
        super::handle_local_input(&owner, &pointer(view::SCROLLBAR, "up", 37, thumb.y)).unwrap();
        assert_eq!(width(&owner), wider + 2);
        assert_eq!(offset(&owner), scrolled);

        // Rows that fit: the edge alone, no thumb.
        let fitting = build_surface(&grouped_state(grouped(), None), 1);
        assert!(region(&fitting, view::SCROLLBAR).is_none());
        assert!(region(&fitting, RESIZE_EDGE_REGION).is_some());
    }

    /// The scrollbar takes its colors from the appearance as Bcode's lists
    /// do: the inactive border for its track, the cursor for its thumb.
    #[test]
    fn the_scrollbar_follows_the_appearance() {
        let mut state = working_cards();
        let mut appearance = bmux_appearance::RuntimeAppearance::default();
        appearance.border.inactive = "#334455".into();
        appearance.cursor = "#ff8800".into();
        appearance.border.active = "#00ffaa".into();
        state.edge_colors = EdgeColors::from_appearance(&appearance);
        let surface = build_surface(&state, 1);
        let column = (1..7)
            .flat_map(|row| painted_row(&surface, row))
            .filter(|(x, ..)| *x == 31)
            .collect::<Vec<_>>();
        let color = |glyph: &str| {
            column
                .iter()
                .find(|(_, text, _)| text == glyph)
                .and_then(|(_, _, style)| style.fg)
        };
        assert_eq!(
            color("▐"),
            Some(RenderColor::Rgb {
                r: 0xff,
                g: 0x88,
                b: 0
            })
        );
        assert_eq!(
            color("▕"),
            Some(RenderColor::Rgb {
                r: 0x33,
                g: 0x44,
                b: 0x55
            })
        );
    }

    /// A sidebar installed like an attach client's, shown at `placement`
    /// and allocated `rect`, remembering its width in `state_dir`.
    struct Installed {
        _presentation: SidebarPresentation,
        layouts: std::sync::Arc<bmux_plugin::layout::PluginLayoutRegistry>,
        allocations: std::sync::Arc<bmux_plugin::layout::AllocationRegistry>,
        input: std::sync::Arc<bmux_plugin::AttachPresentationInputRegistry>,
        surfaces: std::sync::Arc<bmux_plugin::surface::PluginSurfaceRegistry>,
    }

    impl Installed {
        fn new(placement: &str, rect: ExtensionRect, state_dir: &std::path::Path) -> Self {
            let layouts = std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4));
            let allocations =
                std::sync::Arc::new(bmux_plugin::layout::AllocationRegistry::default());
            let input = std::sync::Arc::new(bmux_plugin::AttachPresentationInputRegistry::new());
            let surfaces = std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4));
            let mut settings = toml::map::Map::new();
            settings.insert("visible".into(), toml::Value::Boolean(true));
            settings.insert("placement".into(), toml::Value::String(placement.into()));
            let presentation = SidebarPresentation::install_with_state(
                Some(&toml::Value::Table(settings)),
                layouts.clone(),
                allocations.clone(),
                input.clone(),
                surfaces.clone(),
                Some(state_dir.to_path_buf()),
            )
            .unwrap();
            allocations.notify(&PluginLayoutId::new(OWNER, LAYOUT_ID), rect);
            Self {
                _presentation: presentation,
                layouts,
                allocations,
                input,
                surfaces,
            }
        }

        /// The width the sidebar asks the host for.
        fn requested_width(&self) -> u16 {
            self.layouts
                .requests()
                .into_iter()
                .find_map(|request| match request.operation {
                    bmux_plugin::layout::LayoutOperation::Split {
                        extent: LayoutExtent::Responsive { preferred, .. },
                        ..
                    } => Some(preferred),
                    _ => None,
                })
                .expect("a split request")
        }

        /// The published resize edge region.
        fn edge(&self) -> bmux_plugin::surface::PluginSurfaceRegion {
            self.surfaces
                .owner_snapshot(OWNER)
                .unwrap()
                .surfaces
                .into_iter()
                .flat_map(|surface| surface.interactive_regions)
                .find(|region| region.local_id == RESIZE_EDGE_REGION)
                .expect("a resize edge")
        }

        /// The host allocating a new width, as it does after the request
        /// changes.
        fn reallocate(&self, rect: ExtensionRect) {
            self.allocations
                .notify(&PluginLayoutId::new(OWNER, LAYOUT_ID), rect);
        }

        /// A left-button pointer event on the edge at screen column `col`.
        fn pointer(&self, phase: &str, col: u16) -> bmux_plugin::AttachPresentationInputDispatch {
            let edge = self.edge();
            let event = AttachInputEvent {
                target: Some(bmux_plugin::AttachInputTarget {
                    surface: SURFACE_ID.to_string(),
                    region: RESIZE_EDGE_REGION.to_string(),
                    revision: 0,
                    key: None,
                }),
                hook_id: format!("{OWNER}:{SURFACE_ID}:{RESIZE_EDGE_REGION}"),
                event_kind: "pointer".to_string(),
                phase: phase.to_string(),
                key: None,
                button: Some("left".to_string()),
                col: Some(edge.rect.x),
                row: Some(3),
                wheel_delta: 0,
                modifiers: bmux_plugin::render::AttachInputModifiers::default(),
                focused_pane: None,
                hovered_pane: None,
                screen_col: Some(col),
                screen_row: Some(3),
            };
            self.input.dispatch(&input_endpoint(), &event)
        }

        fn handled(&self, phase: &str, col: u16) -> AttachInputResult {
            match self.pointer(phase, col) {
                bmux_plugin::AttachPresentationInputDispatch::Handled(result) => result,
                _ => panic!("{phase} at {col} was not handled"),
            }
        }
    }

    fn scratch_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bmux-sidebar-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The edge facing the panes shows the resize cursor, and dragging it
    /// sets the width the sidebar asks for, kept within its bounds, which the
    /// next start remembers.
    #[test]
    fn dragging_the_edge_resizes_the_sidebar_and_remembers_it() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        let edge = sidebar.edge();
        assert_eq!((edge.rect.x, edge.rect.w, edge.rect.h), (27, 1, 20));
        assert_eq!(
            edge.cursor,
            bmux_plugin::surface::PluginSurfaceCursor::ResizeHorizontal
        );

        let pressed = sidebar.handled("down", 27);
        assert!(pressed.consumed && pressed.capture_pointer);
        // Past the sidebar into the panes: wider by as much as the pointer moved.
        assert!(sidebar.handled("drag", 39).consumed);
        assert_eq!(sidebar.requested_width(), 40);
        sidebar.reallocate(ExtensionRect::new(0, 0, 40, 20));
        // Within the maximum and the minimum, however far it goes.
        sidebar.handled("drag", 500);
        assert_eq!(sidebar.requested_width(), 60);
        sidebar.handled("drag", 0);
        assert_eq!(sidebar.requested_width(), 16);
        sidebar.handled("drag", 33);
        assert_eq!(sidebar.requested_width(), 34);
        let released = sidebar.handled("up", 33);
        assert!(released.release_capture && released.status_message.is_none());
        // Once released, a drag is no longer the sidebar's.
        assert!(!matches!(
            sidebar.pointer("drag", 50),
            bmux_plugin::AttachPresentationInputDispatch::Handled(result) if result.consumed
        ));
        assert_eq!(sidebar.requested_width(), 34);

        drop(sidebar);
        let again = Installed::new("left", ExtensionRect::new(0, 0, 34, 20), &dir);
        assert_eq!(again.requested_width(), 34);
        drop(again);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// On the right, the edge is the sidebar's first column, and dragging it
    /// left (past the sidebar, where it has no column of its own) widens it.
    #[test]
    fn a_right_sidebar_widens_when_its_edge_is_dragged_left() {
        let dir = scratch_dir();
        let sidebar = Installed::new("right", ExtensionRect::new(72, 0, 28, 20), &dir);
        assert_eq!(sidebar.edge().rect.x, 0);
        sidebar.handled("down", 72);
        sidebar.handled("drag", 62);
        assert_eq!(sidebar.requested_width(), 38);
        sidebar.handled("drag", 80);
        assert_eq!(sidebar.requested_width(), 20);
        sidebar.handled("up", 80);
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `widen` and `narrow` step the width within its bounds and remember it.
    #[test]
    fn widen_and_narrow_commands_step_the_width() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        let run = |name: &str| sidebar.input.command(OWNER, name, &[]).unwrap();
        assert!(run("widen").consumed);
        assert_eq!(sidebar.requested_width(), 30);
        run("narrow");
        run("narrow");
        assert_eq!(sidebar.requested_width(), 26);
        for _ in 0..20 {
            run("narrow");
        }
        assert_eq!(sidebar.requested_width(), 16);
        drop(sidebar);
        assert_eq!(
            std::fs::read_to_string(dir.join(WIDTH_FILE))
                .unwrap()
                .trim(),
            "16"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `invocation` is the workspaces plugin's `activate-tab` for `tab`.
    fn assert_activates(invocation: &bmux_plugin::AttachInputServiceInvocation, tab: Uuid) {
        use bmux_plugin_sdk::TypedServiceEndpoint as _;
        use bmux_workspaces_plugin_api::workspaces_activation_v1::{
            self as activation, client::ActivateTabEndpoint,
        };
        assert_eq!(
            invocation.endpoint.operation,
            ActivateTabEndpoint::OPERATION.to_string()
        );
        assert_eq!(
            invocation.endpoint.interface_id,
            ActivateTabEndpoint::INTERFACE_ID.to_string()
        );
        let payload = bmux_plugin_sdk::service_contract::decode_for(
            &invocation.endpoint.interface_id,
            activation::SERVICE_CONTRACT,
            &invocation.payload,
        )
        .unwrap();
        let request: activation::client::ActivateTabRequest =
            bmux_plugin_sdk::decode_service_message(payload).unwrap();
        assert_eq!(request.context_id, tab);
    }

    /// Clicking a tab's card, or pressing Enter on it, switches to that tab
    /// when the input arrives the way the attach client routes it: through
    /// the presentation's registered endpoint.
    #[test]
    fn clicking_or_entering_a_tab_card_switches_to_it() {
        let input = std::sync::Arc::new(bmux_plugin::AttachPresentationInputRegistry::new());
        let presentation = SidebarPresentation::install(
            Some(&shown_toml()),
            std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4)),
            std::sync::Arc::new(bmux_plugin::layout::AllocationRegistry::default()),
            input.clone(),
            std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4)),
        )
        .unwrap();
        let second = Uuid::from_u128(2);
        let target = {
            let guard = presentation.owner.lock().unwrap();
            test_target(guard.as_ref().unwrap(), second)
        };
        let event =
            |kind: &str, phase: &str, button: Option<&str>, key: Option<&str>| AttachInputEvent {
                target: Some(target.clone()),
                hook_id: format!("{OWNER}:{SURFACE_ID}:{}", tab_region_id(second)),
                event_kind: kind.to_string(),
                phase: phase.to_string(),
                key: key.map(str::to_string),
                button: button.map(str::to_string),
                col: None,
                row: None,
                wheel_delta: 0,
                modifiers: bmux_plugin::render::AttachInputModifiers::default(),
                focused_pane: None,
                hovered_pane: None,
                screen_col: None,
                screen_row: None,
            };
        for event in [
            event("pointer", "down", Some("left"), None),
            event("key", "press", None, Some("enter")),
        ] {
            let bmux_plugin::AttachPresentationInputDispatch::Handled(result) =
                input.dispatch(&input_endpoint(), &event)
            else {
                panic!("{} {} was not handled", event.event_kind, event.phase);
            };
            assert!(result.consumed);
            let invocation = result
                .service_invocation
                .expect("an activate-tab invocation");
            assert_activates(&invocation, second);
        }
        // Other buttons and phases do not switch tabs.
        for event in [
            event("pointer", "down", Some("right"), None),
            event("pointer", "up", Some("left"), None),
        ] {
            let switched = matches!(
                input.dispatch(&input_endpoint(), &event),
                bmux_plugin::AttachPresentationInputDispatch::Handled(result)
                    if result.service_invocation.is_some()
            );
            assert!(!switched, "{} {:?}", event.phase, event.button);
        }
    }

    #[test]
    fn retired_revisions_and_unknown_keys_resolve_to_no_tab() {
        let state = CompanionState::new(shown_settings(), None);
        let target = test_target(&state, Uuid::from_u128(5));
        let event = |target| AttachInputEvent {
            target,
            hook_id: String::new(),
            event_kind: "pointer".to_string(),
            phase: "enter".to_string(),
            button: None,
            key: None,
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: bmux_plugin::render::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
            screen_col: None,
            screen_row: None,
        };
        assert_eq!(
            hit(&state, &event(Some(target.clone()))),
            Some(SidebarHit::Tab(Uuid::from_u128(5)))
        );
        let retired = bmux_plugin::AttachInputTarget {
            revision: 3,
            ..target.clone()
        };
        assert_eq!(hit(&state, &event(Some(retired))), None);
        let unknown = bmux_plugin::AttachInputTarget {
            key: Some(bmux_presentation_scene::scene::HitKey(9)),
            ..target
        };
        assert_eq!(hit(&state, &event(Some(unknown))), None);
        assert_eq!(hit(&state, &event(None)), None);
        assert_eq!(
            region_tab_id(&tab_region_id(Uuid::from_u128(5))),
            Some(Uuid::from_u128(5))
        );
    }

    /// The surface's tab card regions, without the resize edge.
    fn tab_regions(surface: &PluginSurface) -> Vec<&bmux_plugin::surface::PluginSurfaceRegion> {
        surface
            .interactive_regions
            .iter()
            .filter(|region| region.local_id.starts_with("tab:"))
            .collect()
    }

    /// What a surface paints on row `y`: each run's column, text and style.
    fn painted_row(surface: &PluginSurface, y: u16) -> Vec<(u16, String, RenderStyle)> {
        surface
            .ops
            .iter()
            .filter_map(|op| match op {
                RenderOp::TextRun {
                    x,
                    y: row,
                    text,
                    style,
                } if *row == y => Some((*x, text.clone(), *style)),
                _ => None,
            })
            .collect()
    }

    /// The surface's text, row by row, as text runs paint it.
    fn surface_text(surface: &PluginSurface, width: u16, rows: u16) -> Vec<String> {
        let mut grid = vec![vec![' '; usize::from(width)]; usize::from(rows)];
        for op in &surface.ops {
            if let RenderOp::TextRun { x, y, text, .. } = op
                && *y < rows
            {
                for (offset, ch) in text.chars().enumerate() {
                    let column = usize::from(*x) + offset;
                    if column < usize::from(width) {
                        grid[usize::from(*y)][column] = ch;
                    }
                }
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// A tall screen of full status cards keeps its text selection within the
    /// host's limits, and an extreme one still draws, without it.
    #[test]
    fn many_full_cards_publish_within_the_hosts_limits() {
        use bmux_tabs_plugin_api::tabs_status::{GitStatus, TabStatus, TabStatusSnapshot};
        let cards = |rows: u16| {
            let mut state = CompanionState::new(shown_settings(), None);
            state.allocation = Some(ExtensionRect::new(0, 0, 32, rows));
            let ids = (1..=120_u128).map(Uuid::from_u128).collect::<Vec<_>>();
            state.activity.replace(TabStatusSnapshot {
                tabs: ids
                    .iter()
                    .map(|id| TabStatus {
                        tab_id: *id,
                        panes: 2,
                        running_command: Some("cargo nextest run".into()),
                        cwd: Some("/Users/someone/GitHub/project/plugins/sidebar".into()),
                        title: None,
                        git: Some(GitStatus {
                            branch: "feature/robust-themes".into(),
                            dirty: true,
                            ahead: 3,
                            behind: 1,
                        }),
                        last_exit: None,
                        output_at_ms: Some(5),
                        attention_at_ms: None,
                        attention: None,
                        agent: None,
                    })
                    .collect(),
                now_ms: 10,
                revision: 1,
            });
            state.replace_tabs(tabs_list::TabListSnapshot {
                tabs: ids
                    .iter()
                    .enumerate()
                    .map(|(index, id)| tabs_list::TabListEntry {
                        id: *id,
                        name: format!("code-review {index}"),
                        active: false,
                        workspace: "default".to_string(),
                        workspace_id: Uuid::nil(),
                    })
                    .collect(),
                revision: 1,
            });
            let registry = bmux_plugin::surface::PluginSurfaceRegistry::new(4);
            publish_surfaces(&registry, 1, vec![build_surface(&state, 1)]).unwrap();
            registry.owner_snapshot(OWNER).unwrap().surfaces.remove(0)
        };
        // 100 rows of cards (about 45 tabs) keep their text selection.
        assert!(cards(100).metadata.is_some());
        // Far more than the host bounds still draws every visible card.
        let extreme = cards(1_000);
        assert!(extreme.metadata.is_none());
        assert!(extreme.ops.len() > 100);
    }

    /// A surface the host rejects for its metadata is drawn without it.
    #[test]
    fn a_rejected_surface_is_published_without_its_selection() {
        let mut state = CompanionState::new(shown_settings(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 12));
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id: Uuid::from_u128(1),
                name: "only".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: Uuid::nil(),
            }],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        // Every painted run fits the text limit here; the selection metadata,
        // which holds every field's text, does not.
        let longest_run = surface
            .ops
            .iter()
            .filter_map(|op| match op {
                RenderOp::TextRun { text, .. } => Some(text.len()),
                _ => None,
            })
            .max()
            .unwrap();
        let registry = bmux_plugin::surface::PluginSurfaceRegistry::with_limits(
            4,
            4_096,
            longest_run,
            1 << 20,
        );
        publish_surfaces(&registry, 1, vec![surface]).unwrap();
        let published = registry.owner_snapshot(OWNER).unwrap();
        assert!(published.surfaces[0].metadata.is_none());
        assert_ne!(published.surfaces[0].ops.len(), 0);
    }

    #[test]
    fn tabs_show_their_activity_as_status_cards() {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, GitStatus, TabAgent, TabStatus, TabStatusSnapshot,
        };
        let mut state = CompanionState::new(shown_settings(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 12));
        let tab = |index: u128, name: &str, active: bool| tabs_list::TabListEntry {
            id: Uuid::from_u128(index),
            name: name.to_string(),
            active,
            workspace: "default".to_string(),
            workspace_id: Uuid::nil(),
        };
        let status = |index: u128| TabStatus {
            tab_id: Uuid::from_u128(index),
            panes: 1,
            running_command: None,
            cwd: None,
            title: None,
            git: None,
            last_exit: None,
            output_at_ms: None,
            attention_at_ms: None,
            attention: None,
            agent: None,
        };
        let now = 600_000;
        let agent = |name: &str, activity, message: Option<&str>| TabAgent {
            name: name.into(),
            activity,
            message: message.map(str::to_string),
            changed_at_ms: now - 5_000,
            finished_at_ms: None,
        };
        state.activity.replace(TabStatusSnapshot {
            tabs: vec![
                TabStatus {
                    running_command: Some("nvim src/lib.rs".into()),
                    cwd: Some("/work/bmux".into()),
                    git: Some(GitStatus {
                        branch: "main".into(),
                        dirty: true,
                        ahead: 2,
                        behind: 0,
                    }),
                    ..status(1)
                },
                TabStatus {
                    running_command: Some("claude".into()),
                    cwd: Some("/work/bmux/plugins".into()),
                    output_at_ms: Some(now - 1_000),
                    agent: Some(agent("claude", AgentActivity::Working, None)),
                    ..status(2)
                },
                TabStatus {
                    last_exit: Some(101),
                    cwd: Some("/work/build".into()),
                    ..status(3)
                },
                TabStatus {
                    running_command: Some("codex".into()),
                    output_at_ms: Some(now - 9_000),
                    agent: Some(agent(
                        "codex",
                        AgentActivity::Blocked,
                        Some("approve edit?"),
                    )),
                    ..status(4)
                },
            ],
            now_ms: now,
            revision: 1,
        });
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![
                tab(1, "editor", true),
                tab(2, "claude", false),
                tab(3, "build", false),
                tab(4, "review", false),
            ],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        let text = surface_text(&surface, 32, 10);
        assert_eq!(
            text,
            [
                " TABS                         4",
                "▎ ○ editor                 nvim",
                "▎   /work/bmux · main* ↑2",
                "  ◉ claude              working",
                "    /work/bmux/plugins",
                "  ▲ build              exit 101",
                "    /work/build",
                "  ◆ review          needs input",
                "    approve edit?",
                "",
            ],
            "{}",
            text.join("\n")
        );
    }

    /// A card's painted state glyph with its color, and its summary.
    type PaintedCard = (Option<(String, Option<RenderColor>)>, String);

    /// Each named card's painted state glyph with its color, and the last
    /// word of its first line (its summary).
    fn painted_cards(state: &CompanionState, names: &[&str]) -> Vec<PaintedCard> {
        let surface = build_surface(state, 1);
        let text = surface_text(&surface, 32, 10);
        names
            .iter()
            .map(|name| {
                let row = text
                    .iter()
                    .position(|line| line.split_whitespace().any(|word| word == *name))
                    .and_then(|row| u16::try_from(row).ok())
                    .unwrap_or_else(|| panic!("no card for {name}: {text:?}"));
                // The glyph painted last over the title is the one shown.
                let glyph = surface.ops.iter().rev().find_map(|op| match op {
                    RenderOp::TextRun {
                        x: 2,
                        y,
                        text,
                        style,
                    } if *y == row => Some((text.clone(), style.fg)),
                    _ => None,
                });
                let summary = text[usize::from(row)]
                    .split_whitespace()
                    .last()
                    .unwrap_or_default()
                    .to_string();
                (glyph, summary)
            })
            .collect()
    }

    /// Selecting a tab moves the selection, never what each agent's card
    /// says it is doing: working, waiting for an answer, or stopped.
    #[test]
    fn switching_tabs_keeps_every_agents_painted_state() {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, TabAgent, TabStatus, TabStatusSnapshot,
        };
        let now = 600_000;
        let agents = [
            ("working", AgentActivity::Working, None),
            ("asking", AgentActivity::Blocked, None),
            ("stopped", AgentActivity::Idle, Some(now - 60_000)),
        ];
        let mut state = CompanionState::new(shown_settings(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 10));
        state.activity.replace(TabStatusSnapshot {
            tabs: (1..=3_u128)
                .zip(&agents)
                .map(|(index, (_, activity, finished_at_ms))| TabStatus {
                    tab_id: Uuid::from_u128(index),
                    panes: 1,
                    running_command: Some("claude".into()),
                    cwd: None,
                    title: None,
                    git: None,
                    last_exit: None,
                    output_at_ms: Some(now - 500),
                    attention_at_ms: None,
                    attention: None,
                    agent: Some(TabAgent {
                        name: "claude".into(),
                        activity: *activity,
                        message: (*activity == AgentActivity::Blocked)
                            .then(|| "Which layout?".into()),
                        changed_at_ms: now - 60_000,
                        finished_at_ms: *finished_at_ms,
                    }),
                })
                .collect(),
            now_ms: now,
            revision: 1,
        });
        let names = agents.map(|(name, ..)| name);
        let painted = |state: &CompanionState| painted_cards(state, &names);
        let select = |state: &mut CompanionState, selected: u128| {
            state.replace_tabs(tabs_list::TabListSnapshot {
                tabs: (1..=3_u128)
                    .zip(&agents)
                    .map(|(index, (name, ..))| tabs_list::TabListEntry {
                        id: Uuid::from_u128(index),
                        name: (*name).to_string(),
                        active: index == selected,
                        workspace: "default".to_string(),
                        workspace_id: Uuid::nil(),
                    })
                    .collect(),
                revision: 1,
            });
            if let Some(active) = state.snapshot.tabs.iter().find(|tab| tab.active) {
                state.activity.mark_viewed(active.id);
            }
        };
        select(&mut state, 1);
        let first = painted(&state);
        let summaries = first
            .iter()
            .map(|(_, summary)| summary.as_str())
            .collect::<Vec<_>>();
        assert_eq!(summaries, ["working", "input", "stopped"]);
        let glyphs = first
            .iter()
            .map(|(glyph, _)| glyph.clone().unwrap())
            .collect::<Vec<_>>();
        for (index, glyph) in glyphs.iter().enumerate() {
            for other in &glyphs[index + 1..] {
                assert_ne!(glyph.0, other.0, "three different glyphs: {glyphs:?}");
                assert_ne!(glyph.1, other.1, "three different colors: {glyphs:?}");
            }
        }
        for selected in [2, 3, 1, 3] {
            select(&mut state, selected);
            assert_eq!(painted(&state), first, "after selecting tab {selected}");
        }
    }

    #[test]
    fn home_and_end_move_the_highlight_to_the_first_and_last_tab() {
        let mut state = CompanionState::new(shown_settings(), None);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (1..=4)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: index == 2,
                    workspace: "default".to_string(),
                    workspace_id: Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        state.surfaces = Some(std::sync::Arc::new(
            bmux_plugin::surface::PluginSurfaceRegistry::new(8),
        ));
        let target = test_target(&state, Uuid::from_u128(2));
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
        let key = |name: &str| AttachInputEvent {
            target: Some(target.clone()),
            hook_id: String::new(),
            event_kind: "key".to_string(),
            phase: "press".to_string(),
            button: None,
            key: Some(name.to_string()),
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: bmux_plugin::render::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
            screen_col: None,
            screen_row: None,
        };
        let hovered = |owner: &CompanionHandle| {
            owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .cursor
                .hovered()
                .copied()
        };
        assert!(update_keyboard(&owner, &key("end")).unwrap().consumed);
        assert_eq!(hovered(&owner), Some(SidebarHit::Tab(Uuid::from_u128(4))));
        assert!(update_keyboard(&owner, &key("home")).unwrap().consumed);
        assert_eq!(hovered(&owner), Some(SidebarHit::Tab(Uuid::from_u128(1))));
        assert!(update_keyboard(&owner, &key("x")).is_none());
    }

    #[test]
    fn rejected_interaction_publication_restores_scroll_and_hover() {
        let mut state = CompanionState::new(
            Settings {
                maximum_visible_items: 1,
                ..shown_settings()
            },
            None,
        );
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (1..=3)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        // Reject all surface publications without changing the production publisher.
        state.surfaces = Some(std::sync::Arc::new(
            bmux_plugin::surface::PluginSurfaceRegistry::new(0),
        ));
        let target = test_target(&state, Uuid::from_u128(1));
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(state)));
        let mut event = AttachInputEvent {
            target: Some(target),
            hook_id: String::new(),
            event_kind: "pointer".to_string(),
            phase: "enter".to_string(),
            button: None,
            key: None,
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: bmux_plugin::render::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
            screen_col: None,
            screen_row: None,
        };
        assert!(!update_hover(&owner, &event));
        assert_eq!(
            owner.lock().unwrap().as_ref().unwrap().cursor.hovered(),
            None
        );
        event.phase = "wheel".to_string();
        event.wheel_delta = -1;
        assert!(!update_scroll(&owner, &event));
        assert_eq!(owner.lock().unwrap().as_ref().unwrap().scroll_offset(), 0);
        event.event_kind = "key".to_string();
        event.phase = "press".to_string();
        event.key = Some("down".to_string());
        assert!(!update_keyboard(&owner, &event).unwrap().dirty);
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert_eq!(state.scroll_offset(), 0);
            assert_eq!(state.cursor.hovered(), None);
            state.surfaces = Some(std::sync::Arc::new(
                bmux_plugin::surface::PluginSurfaceRegistry::new(1),
            ));
            drop(guard);
        }
        // A retry must apply exactly one navigation, not accumulate failed moves.
        assert!(update_keyboard(&owner, &event).unwrap().dirty);
        let guard = owner.lock().unwrap();
        let state = guard.as_ref().unwrap();
        assert_eq!(
            state.cursor.hovered(),
            Some(&SidebarHit::Tab(Uuid::from_u128(2)))
        );
        assert!(state.scroll_offset() > 0);
        drop(guard);
    }

    #[test]
    fn measured_scroll_clips_partial_items_and_reveals_full_extent() {
        let mut state = CompanionState::new(
            Settings {
                maximum_visible_items: 1,
                description_template: "description".to_string(),
                ..shown_settings()
            },
            None,
        );
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (0..3)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        // One card (a title and its description) fits; three are listed.
        assert!(state.scroll_rows(1));
        assert_eq!(state.list_height(&state.item_keys()), 2);
        assert_eq!(state.list.borrow().total_height(), 6);
        let surface = build_surface(&state, 1);
        assert_eq!(tab_regions(&surface).len(), 2);
        // The rows overflow, so the edge's column carries the scrollbar.
        assert_eq!(
            tab_regions(&surface)[0].rect,
            ExtensionRect::new(0, 1, state.settings.width - 1, 1)
        );
        assert_eq!(
            tab_regions(&surface)[1].rect,
            ExtensionRect::new(0, 2, state.settings.width - 1, 1)
        );
        assert!(
            surface
                .ops
                .iter()
                .filter_map(|op| match op {
                    RenderOp::TextRun { y, text, .. } if !text.trim().is_empty() => Some(*y),
                    _ => None,
                })
                .all(|y| y <= 2)
        );
        state.reveal_item(SidebarHit::Tab(Uuid::from_u128(2)));
        assert_eq!(state.scroll_offset(), 4);
        assert_eq!(tab_regions(&build_surface(&state, 2)).len(), 1);
        assert!(state.resize(ExtensionRect::new(0, 0, 12, 5)));
        assert_eq!(state.list_height(&state.item_keys()), 3);
        assert_eq!(state.allocated_width(), 12);
        // Narrower, the descriptions wrap to two rows: the offset still fits.
        assert_eq!(state.scroll_offset(), 4);
        assert!(!state.resize(ExtensionRect::new(0, 0, 12, 5)));
        assert!(state.resize(ExtensionRect::new(0, 0, 12, 1)));
        assert_eq!(
            tab_regions(&build_surface(&state, 3)),
            [] as [&bmux_plugin::surface::PluginSurfaceRegion; 0]
        );
    }

    #[test]
    fn virtual_tab_realigns_to_active_and_bounds_projected_items() {
        let settings = Settings {
            maximum_visible_items: 2,
            ..shown_settings()
        };
        let mut state = CompanionState::new(settings, None);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (0..5)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: index == 4,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        assert_eq!(state.scroll_offset(), 3);
        let surface = build_surface(&state, 1);
        assert_eq!(tab_regions(&surface).len(), 2);
        assert_eq!(
            tab_regions(&surface)[1].local_id,
            format!("tab:{}", Uuid::from_u128(4))
        );
    }

    #[test]
    fn one_semantic_fact_changes_only_its_target_card_operation() {
        let producer = format!("sidebar-test-{}", Uuid::from_u128(30));
        let first = Uuid::from_u128(31);
        let second = Uuid::from_u128(32);
        let facts = std::sync::Arc::new(bmux_presentation_state::PresentationFactRegistry::new());
        let mut state = CompanionState::new(shown_settings(), None);
        state.facts = Some(FactSource::Registry(facts.clone()));
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![
                tabs_list::TabListEntry {
                    id: first,
                    name: "one".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
                tabs_list::TabListEntry {
                    id: second,
                    name: "two".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
            ],
            revision: 1,
        });
        let before = build_surface(&state, 1);
        facts
            .publish(
                &producer,
                bmux_presentation_state::PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![PresentationFact {
                        entity: PresentationEntityRef::new("bmux.tabs", second.to_string()),
                        key: "activity".to_string(),
                        role: PresentationFactRole::Warning,
                        short_text: "waiting".to_string(),
                        detail_text: None,
                        icon_id: None,
                        priority: 1,
                    }],
                },
            )
            .unwrap();
        let after = build_surface(&state, 2);
        // Only the card the fact is about changes.
        assert_eq!(painted_row(&before, 1), painted_row(&after, 1));
        assert_ne!(painted_row(&before, 2), painted_row(&after, 2));
        assert!(facts.remove_producer(&producer));
    }

    #[test]
    #[ignore = "manual presentation performance baseline; run with --release --ignored --nocapture"]
    fn sidebar_projection_performance_baseline() {
        use std::time::Instant;

        let settings = Settings {
            maximum_visible_items: 32,
            ..shown_settings()
        };
        let mut state = CompanionState::new(settings, None);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (0..2_000)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: index == 1_999,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        let iterations = 10_000_u32;
        let started = Instant::now();
        for revision in 1..=iterations {
            std::hint::black_box(build_surface(&state, u64::from(revision)));
        }
        let average_ns = started.elapsed().as_nanos() / u128::from(iterations);
        eprintln!("sidebar 2,000-tab/32-visible projection average: {average_ns} ns");
        assert!(average_ns < 70_000, "projection exceeded 70 us budget");
    }

    #[test]
    fn single_and_large_tab_lists_remain_bounded() {
        let settings = Settings {
            maximum_visible_items: 32,
            ..shown_settings()
        };
        let mut state = CompanionState::new(settings, None);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id: Uuid::from_u128(50),
                name: "single".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        assert_eq!(tab_regions(&build_surface(&state, 1)).len(), 1);

        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: (0..2_000)
                .map(|index| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("tab-{index}"),
                    active: index == 1_999,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                })
                .collect(),
            revision: 2,
        });
        let surface = build_surface(&state, 2);
        assert_eq!(tab_regions(&surface).len(), 32);
        assert_eq!(state.scroll_offset(), 1_968);
    }

    #[test]
    fn content_height_bounds_background_paint() {
        let mut state = CompanionState::new(
            Settings {
                content_height: true,
                ..shown_settings()
            },
            None,
        );
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id: Uuid::from_u128(40),
                name: "one".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        let metadata = surface
            .metadata
            .as_ref()
            .expect("real sidebar producer metadata");
        assert!(metadata.selection.as_ref().unwrap().validate().is_ok());
        assert_ne!(metadata.selection.as_ref().unwrap().fragments(), []);
        assert!(metadata.text.values().any(|text| text.contains("one")));
        assert_ne!(metadata.semantics.as_ref().unwrap().regions(), []);
        // The heading, the one card and a spacer: the background covers
        // exactly those rows.
        let rows = surface
            .ops
            .iter()
            .filter_map(|op| match op {
                RenderOp::TextRun { y, .. } => Some(*y),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(rows, std::collections::BTreeSet::from([0, 1, 2]));
    }

    #[test]
    fn surface_uses_one_stable_region_per_tab() {
        let mut state = CompanionState::new(shown_settings(), None);
        let id = Uuid::from_u128(9);
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: vec![tabs_list::TabListEntry {
                id,
                name: "main".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        assert_eq!(surface.interactive_regions[0].local_id, format!("tab:{id}"));
        assert!(surface.accepts_input);
    }
}

#[cfg(test)]
mod embedded_declaration_identity_tests {
    #[test]
    fn definition_and_selected_manifest_have_one_identity() {
        let definition = super::plugin_definition();
        definition.validate().expect("definition validates");
        let embedded = definition
            .metadata
            .manifest_toml
            .as_deref()
            .expect("embedded declaration");
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(embedded).expect("valid manifest");
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        assert_eq!(definition.metadata.contracts, manifest.contracts);
        assert_eq!(
            definition.metadata.required_capabilities,
            manifest
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );

        // Deterministic verifier fixtures, not claims about built executable bytes.
        // Real selected artifact hashing remains the loader's responsibility.
        let expected_digest = "a".repeat(64);
        let wrong_digest = "b".repeat(64);
        let pinned = format!("artifact_sha256 = \"{expected_digest}\"\n{embedded}");
        let mut inventory = bmux_plugin::PluginCandidateInventory::new();
        inventory
            .add_bundled_manifest(&pinned)
            .expect("inventory declaration");
        for role in &manifest.component_roles {
            let selected = inventory
                .select(&bmux_plugin::PluginSelectionRequest {
                    plugin_id: manifest.id.clone(),
                    role: *role,
                    preference: bmux_plugin::PluginCandidatePreference::Bundled,
                    plugin_api: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    native_abi: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    contracts: manifest.contracts.clone(),
                })
                .expect("select declared role and contracts");
            selected
                .validate_embedded_metadata(&definition.metadata, &expected_digest)
                .expect("selected declaration and embedded identity match");
            assert!(
                selected
                    .validate_embedded_metadata(&definition.metadata, &wrong_digest)
                    .is_err()
            );
            let mut conflicting = definition.metadata.clone();
            conflicting.version.push_str("-conflict");
            assert!(
                selected
                    .validate_embedded_metadata(&conflicting, &expected_digest)
                    .is_err()
            );
        }
    }
}
