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
mod look;
mod personal_tabs;
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
use bmux_tabs_plugin_api::{personal_view, tabs_list, tabs_status};
use bmux_tui_components::panel_window::{PanelShape, WindowControl, WindowControls};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceVisibility {
    Manual,
    FollowFocus,
    FollowFocusAndAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtherWorkspaceTabs {
    All,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    placement: Placement,
    grouping: Grouping,
    scope: Scope,
    workspace_visibility: WorkspaceVisibility,
    pinned_workspaces: std::collections::BTreeSet<String>,
    other_workspace_tabs: OtherWorkspaceTabs,
    width: u16,
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
    working_indicator: WorkingIndicator,
    density: Density,
    /// Whether to show Nerd Font icons (folder and branch); they need a Nerd
    /// Font in the terminal.
    icons: bool,
    /// Colors chosen for workspaces by name; others get one from their name.
    workspace_colors: std::collections::BTreeMap<String, RenderColor>,
}

/// How a working tab's glyph shows that it works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkingIndicator {
    /// A spinning glyph.
    Spinner,
    /// The still `◉`.
    Static,
}

/// How much each card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    /// The title line, then what the tab asks or where it is.
    Comfortable,
    /// The title line only.
    Compact,
}

/// The default description: the tab's detail, rendered with its parts styled.
const DEFAULT_DESCRIPTION: &str = "{detail}";

/// Nerd Font icons (private-use code points) before the branch and the
/// directory, shown with `icons = true`, the default.
const BRANCH_ICON: &str = "\u{e0a0} ";
const FOLDER_ICON: &str = "\u{f07b} ";

/// Below this width, cards are one line whatever the density, and their
/// state shows only in their glyph.
const COMPACT_BELOW_WIDTH: u16 = 20;
/// Below this width, state pills and ages say less.
const PILL_SHORT_BELOW_WIDTH: u16 = 28;

/// Milliseconds on this client's clock, for animation frames.
fn clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
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
            maximum_width: 60,
            order: 200,
            show_index: true,
            heading: "Tabs".to_string(),
            title_template: "{state_icon} {name}".to_string(),
            description_template: DEFAULT_DESCRIPTION.to_string(),
            status_template: String::new(),
            maximum_visible_items: 20,
            content_height: false,
            collapse_below_width: 80,
            collapsed_width: 2,
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
            workspace_visibility: WorkspaceVisibility::Manual,
            pinned_workspaces: std::collections::BTreeSet::new(),
            other_workspace_tabs: OtherWorkspaceTabs::All,
            working_indicator: WorkingIndicator::Spinner,
            density: Density::Comfortable,
            icons: true,
            workspace_colors: std::collections::BTreeMap::new(),
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
        parse_widths(table, &mut settings)?;
        parse_organization(table, &mut settings)?;
        parse_look(table, &mut settings)?;
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
            ("icons", &mut settings.icons),
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

/// `width`, `maximum_width`, `collapse_below_width` and `collapsed_width`,
/// and the removed `minimum_width`.
fn parse_widths(
    table: &toml::map::Map<String, toml::Value>,
    settings: &mut Settings,
) -> Result<(), PluginCommandError> {
    if table.contains_key("minimum_width") {
        tracing::warn!(
            "bmux.sidebar minimum_width was removed and is ignored: the sidebar narrows to \
                 collapsed_width, where it becomes a status strip"
        );
    }
    for (key, target) in [
        ("width", &mut settings.width),
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
    // The strip must be narrower than the expanded sidebar: a sidebar
    // allocated no wider than `collapsed_width` is drawn as the strip, and
    // dragging or narrowing to it minimizes.
    if settings.collapsed_width < 2
        || settings.collapsed_width >= settings.width
        || settings.width > settings.maximum_width
    {
        return Err(PluginCommandError::invalid_arguments(
            "bmux.sidebar requires 2 <= collapsed_width < width <= maximum_width",
        ));
    }
    Ok(())
}

/// `working_indicator`, `density` and `workspace_colors`: how the sidebar looks.
fn parse_look(
    table: &toml::map::Map<String, toml::Value>,
    settings: &mut Settings,
) -> Result<(), PluginCommandError> {
    if let Some(indicator) = table.get("working_indicator") {
        settings.working_indicator = match indicator.as_str() {
            Some("spinner") => WorkingIndicator::Spinner,
            Some("static") => WorkingIndicator::Static,
            _ => {
                return Err(PluginCommandError::invalid_arguments(format!(
                    "bmux.sidebar working_indicator must be 'spinner' or 'static', got {indicator}"
                )));
            }
        };
    }
    if let Some(density) = table.get("density") {
        settings.density = match density.as_str() {
            Some("comfortable") => Density::Comfortable,
            Some("compact") => Density::Compact,
            _ => {
                return Err(PluginCommandError::invalid_arguments(format!(
                    "bmux.sidebar density must be 'comfortable' or 'compact', got {density}"
                )));
            }
        };
    }
    if let Some(colors) = table.get("workspace_colors") {
        let colors = colors.as_table().ok_or_else(|| {
            PluginCommandError::invalid_arguments(
                "bmux.sidebar workspace_colors must map workspace names to colors",
            )
        })?;
        for (name, color) in colors {
            let color = color
                .as_str()
                .ok_or_else(|| "not a string".to_string())
                .and_then(look::parse_color)
                .map_err(|error| {
                    PluginCommandError::invalid_arguments(format!(
                        "bmux.sidebar workspace_colors.{name}: {error}"
                    ))
                })?;
            settings.workspace_colors.insert(name.clone(), color);
        }
    }
    Ok(())
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
    if let Some(mode) = table.get("workspace_visibility") {
        settings.workspace_visibility = match mode.as_str() {
            Some("manual") => WorkspaceVisibility::Manual,
            Some("follow_focus") => WorkspaceVisibility::FollowFocus,
            Some("follow_focus_and_attention") => WorkspaceVisibility::FollowFocusAndAttention,
            _ => {
                return Err(PluginCommandError::invalid_arguments(
                    "bmux.sidebar workspace_visibility must be 'manual', 'follow_focus', or 'follow_focus_and_attention'",
                ));
            }
        };
    }
    if let Some(mode) = table.get("other_workspace_tabs") {
        settings.other_workspace_tabs = match mode.as_str() {
            Some("all") => OtherWorkspaceTabs::All,
            Some("active") => OtherWorkspaceTabs::Active,
            _ => {
                return Err(PluginCommandError::invalid_arguments(
                    "bmux.sidebar other_workspace_tabs must be 'all' or 'active'",
                ));
            }
        };
    }
    if let Some(pinned) = table.get("pinned_workspaces") {
        let incompatible = || {
            PluginCommandError::invalid_arguments(
                "bmux.sidebar pinned_workspaces must be an array of workspace names",
            )
        };
        for name in pinned.as_array().ok_or_else(incompatible)? {
            settings
                .pinned_workspaces
                .insert(name.as_str().ok_or_else(incompatible)?.to_string());
        }
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
    /// This attachment's projection of its personal tab view, when it has
    /// one; it decides which tabs are listed and in what order.
    personal: Option<personal_view::PersonalViewProjection>,
    snapshot: tabs_list::TabListSnapshot,
    /// The highlighted row, moved by the pointer (hover) and the keyboard.
    cursor: ItemCursor<SidebarHit>,
    /// Manual group choices. A true value collapses the group.
    group_overrides: std::collections::BTreeMap<Uuid, bool>,
    /// Most recent focused workspace, retained through gaps in the catalog.
    last_focused_workspace: Option<Uuid>,
    /// Where manual group choices (collapsed and expanded) are remembered,
    /// when they are.
    groups_memory: Option<std::path::PathBuf>,
    /// What each tab is doing, from the tabs plugin's `tabs-status`.
    activity: activity::ActivityView,
    /// The rows' measurements and scroll position.
    list: std::cell::RefCell<bmux_tui_components::virtual_list::VirtualListState<SidebarHit>>,
    /// What each published revision's region keys mean.
    hits: std::cell::RefCell<HitTables<SidebarHit>>,
    /// Bumped whenever what the rows are made of changes, other than tab
    /// status: the tab list or the manual group choices.
    rows_epoch: u64,
    /// The listed rows, for the `rows_epoch` and activity generation they
    /// were made from.
    rows_cache: std::cell::RefCell<Option<RowsCache>>,
    /// The last expanded width, which the restore control returns to.
    width: u16,
    width_mode: WidthMode,
    /// Restored from minimized: if the host still allocates a strip (the
    /// terminal is below `collapse_below_width`), expand it explicitly, so
    /// one restore shows the tabs.
    expand_if_strip: bool,
    /// Where the width is remembered, when it is.
    width_memory: Option<std::path::PathBuf>,
    /// A drag of the resize edge in progress.
    resize: Option<ResizeDrag>,
    /// How the pointer and keyboard are engaged with the sidebar.
    engaged: Engagement,
    /// The colors of the column facing the panes, from the appearance.
    edge_colors: EdgeColors,
    /// The sidebar's colors, from the runtime's theme.
    palette: look::Palette,
}

/// Listed rows, with the `rows_epoch` and activity generation they were
/// made from.
type RowsCache = ((u64, u64), std::sync::Arc<[SidebarRow]>);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum WidthMode {
    #[default]
    Responsive,
    Collapsed,
    /// Explicitly restored while below the automatic breakpoint.
    Expanded,
}

/// How the pointer and keyboard are engaged with the sidebar's chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Engagement {
    /// The pointer is over the resize edge.
    edge_hovered: bool,
    control_hovered: Option<WindowControl>,
    /// The sidebar has keyboard focus in this client.
    focused: bool,
}

/// A drag of the sidebar's resize edge: where the pointer pressed, on the
/// screen, and the width then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResizeDrag {
    start_col: u16,
    start_width: u16,
    restore_width: u16,
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
/// Columns an explicitly expanded sidebar leaves the panes at least, when the
/// terminal is too narrow for its whole width.
const PANE_RESERVE: u16 = 20;
/// The file in the local state directory that remembers the width.
const WIDTH_FILE: &str = "sidebar-width";

/// The typed meaning of a sidebar region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SidebarHit {
    Control(WindowControl),
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
        SidebarHit::Control(control) => format!("{}.action.{}", view::WINDOW_ID, control.id()),
    }
}

fn region_item(region: &str) -> Option<SidebarHit> {
    if region == RESIZE_EDGE_REGION {
        return Some(SidebarHit::ResizeEdge);
    }
    if region == view::SCROLLBAR {
        return Some(SidebarHit::Scrollbar);
    }
    if let Some((id, action)) = region.split_once(".action.")
        && id == view::WINDOW_ID
    {
        return WindowControl::from_id(action).map(SidebarHit::Control);
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

/// Where a card is listed and how it is laid out.
#[derive(Debug, Clone, Copy)]
struct CardPlace {
    /// Its row in the list.
    row: usize,
    hovered: bool,
    /// The revision its fields' selection ids belong to.
    revision: u64,
    /// The cells its detail line may use.
    detail_width: usize,
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
        let group_overrides = groups_memory
            .as_deref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        let (id, collapsed) = line
                            .strip_prefix('+')
                            .map_or((line, true), |id| (id, false));
                        Uuid::parse_str(id).ok().map(|id| (id, collapsed))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let width = width_memory
            .as_deref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| text.trim().parse::<u16>().ok())
            .map_or(settings.width, |width| {
                width.clamp(settings.collapsed_width + 1, settings.maximum_width)
            });
        Self {
            group_overrides,
            last_focused_workspace: None,
            groups_memory,
            rows_epoch: 0,
            rows_cache: std::cell::RefCell::new(None),
            width,
            width_mode: WidthMode::default(),
            expand_if_strip: false,
            width_memory,
            resize: None,
            engaged: Engagement::default(),
            edge_colors: EdgeColors::default(),
            palette: look::Palette::default(),
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
            personal: None,
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
            None => self.requested_width(),
        }
    }

    const fn requested_width(&self) -> u16 {
        if matches!(self.width_mode, WidthMode::Collapsed) {
            self.settings.collapsed_width
        } else {
            self.width
        }
    }

    const fn is_strip(&self) -> bool {
        matches!(self.width_mode, WidthMode::Collapsed)
            || self.allocated_width() <= self.settings.collapsed_width
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

    /// Resize to the strip floor or to an expanded width.
    fn set_width(&mut self, width: u16) -> bool {
        let previous = (self.width, self.width_mode);
        if width <= self.settings.collapsed_width {
            self.width_mode = WidthMode::Collapsed;
        } else {
            self.restore();
            self.width = width.min(self.settings.maximum_width);
        }
        previous != (self.width, self.width_mode)
    }

    /// Leave the strip, or keep the width mode when already expanded. A
    /// strip the host chose (the terminal is narrower than
    /// `collapse_below_width`) expands explicitly. A minimized sidebar goes
    /// back to the responsive width; should the host still give it a strip,
    /// the next allocation expands it explicitly, so one restore always
    /// shows the tabs.
    const fn restore(&mut self) {
        self.width_mode = match self.width_mode {
            WidthMode::Expanded => WidthMode::Expanded,
            WidthMode::Responsive if self.visible() && self.is_strip() => WidthMode::Expanded,
            WidthMode::Collapsed => {
                self.expand_if_strip = true;
                WidthMode::Responsive
            }
            WidthMode::Responsive => WidthMode::Responsive,
        };
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
        if std::mem::take(&mut self.expand_if_strip)
            && self.width_mode == WidthMode::Responsive
            && rect.w <= self.settings.collapsed_width
        {
            self.width_mode = WidthMode::Expanded;
            if let Err(error) = publish_layout(self) {
                tracing::warn!(%error, "sidebar restore publication failed");
            }
        }
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
        let palette = self.palette;
        let rows = self.rows();
        let keys = rows
            .iter()
            .map(|row| row.item(&self.snapshot.tabs))
            .collect::<Vec<_>>();
        // A card's detail line is laid out at the list's width, one column
        // short of the sidebar's while the scrollbar shows.
        let ((_, list_width), _) = self.list_columns(self.list_height(&keys));
        let detail_width =
            usize::try_from(view::detail_width(u64::from(list_width))).unwrap_or(usize::MAX);
        rows.iter()
            .zip(keys)
            .enumerate()
            .map(|(index, (row, item))| {
                let hovered = self.cursor.hovered() == Some(&item);
                // The last row of a group sets it off from the next group.
                let spacer = matches!(rows.get(index + 1), Some(SidebarRow::Group { .. }));
                let row = match row {
                    SidebarRow::Tab(tab) => {
                        let mut card = self.card(
                            *tab,
                            CardPlace {
                                row: index,
                                hovered,
                                revision,
                                detail_width,
                            },
                            &palette,
                        );
                        card.spacer = spacer;
                        view::RowView::Card(card)
                    }
                    SidebarRow::Group {
                        name,
                        tabs,
                        collapsed,
                        ..
                    } => {
                        let mut header = self.header(name, tabs, *collapsed, hovered, &palette);
                        header.spacer = spacer;
                        view::RowView::Header(header)
                    }
                };
                (item, row)
            })
            .collect()
    }

    /// A tab's card: its state glyph (a spinner while it works) and name, its
    /// state at the right (a solid pill when it needs the user or failed),
    /// and below, what it asks or where it is and its git status. The shown
    /// tab is tinted with a bar; under a workspace header, a rail in the
    /// workspace's color runs down the first column.
    fn card(&self, tab_index: usize, place: CardPlace, palette: &look::Palette) -> view::CardRow {
        use bmux_plugin::render_paint::tui_style;
        let CardPlace {
            row,
            hovered,
            revision,
            detail_width,
        } = place;
        let tab = &self.snapshot.tabs[tab_index];
        let fact = self.tab_fact(tab);
        let activity = self
            .activity
            .activity(tab.id)
            .map(|activity| activity.fitted(detail_width));
        let render = |template: &str| {
            render_template(
                template,
                TemplateTab {
                    tab,
                    index: tab_index,
                    show_index: self.settings.show_index,
                    icons: self.settings.icons,
                },
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
                    palette.strong
                } else {
                    palette.text
                }
            },
            |fact| fact_style(fact.role, palette.text),
        );
        let title = render(&self.settings.title_template);
        let (glyph, color) = state_glyph(activity.as_ref());
        let shown_glyph = match activity.as_ref().map(|activity| activity.state) {
            Some(activity::TabState::Working)
                if self.settings.working_indicator == WorkingIndicator::Spinner =>
            {
                look::spinner_frame(clock_ms())
            }
            _ => glyph,
        };
        let mut spans = Vec::new();
        let rest = match (&activity, title.strip_prefix(glyph)) {
            (Some(_), Some(rest)) => {
                spans.push(bmux_tui::text::Span::styled(
                    shown_glyph.to_string(),
                    tui_style(RenderStyle::new().foreground(color)),
                ));
                rest.to_string()
            }
            _ => title,
        };
        spans.push(bmux_tui::text::Span::styled(rest, tui_style(title_style)));
        let trailing = activity
            .as_ref()
            .and_then(|activity| self.card_state(activity, palette));
        let field = |name: &str| view::FieldId {
            id: selection_id(row, name),
            revision,
        };
        let workspace = (self.settings.grouping == Grouping::Workspace)
            .then(|| look::workspace_color(&tab.workspace, &self.settings.workspace_colors));
        let bar = if tab.active {
            Some((
                "▎",
                tui_style(RenderStyle::new().foreground(workspace.unwrap_or(palette.accent))),
            ))
        } else {
            workspace.map(|color| ("▏", tui_style(RenderStyle::new().foreground(color))))
        };
        view::CardRow {
            title: bmux_tui::text::Line::from_spans(spans),
            trailing,
            description: self.card_detail(activity.as_ref(), &render, detail_width, palette),
            status: render(&self.settings.status_template),
            muted: tui_style(palette.muted),
            chrome: view::RowChrome {
                card: card.map(tui_style),
                bar,
                base: tui_style(palette.base),
            },
            fields: [field("title"), field("description"), field("status")],
            spacer: false,
        }
    }

    /// Whether cards are one line: by choice, or when the sidebar is too
    /// narrow for a useful second line.
    fn compact(&self) -> bool {
        self.settings.density == Density::Compact || self.allocated_width() < COMPACT_BELOW_WIDTH
    }

    /// A card's state at the right of its title. Use a solid badge for
    /// input, failure, and completion, with the age beside the state when it fits.
    fn card_state(
        &self,
        activity: &activity::TabActivity,
        palette: &look::Palette,
    ) -> Option<bmux_tui::text::Line> {
        use bmux_plugin::render_paint::tui_style;
        use bmux_tui::text::Span;
        // Too narrow for the name and a summary: the glyph says the state.
        if self.allocated_width() < COMPACT_BELOW_WIDTH {
            return None;
        }
        let summary = activity.summary.clone()?;
        let narrow = self.allocated_width() < PILL_SHORT_BELOW_WIDTH;
        let state_style = RenderStyle::new().foreground(activity.state.color());
        let label = match activity.state {
            activity::TabState::Blocked | activity::TabState::Failed | activity::TabState::Done => {
                let text = if narrow && activity.state == activity::TabState::Blocked {
                    "input"
                } else {
                    &summary
                };
                let pill = RenderStyle::new()
                    .background(activity.state.color())
                    .foreground(palette.on_pill)
                    .bold();
                Span::styled(format!(" {text} "), tui_style(pill))
            }
            activity::TabState::Idle => Span::styled(summary, tui_style(palette.muted)),
            activity::TabState::Working
            | activity::TabState::Shell
            | activity::TabState::Stopped => Span::styled(summary, tui_style(state_style)),
        };
        let mut spans = vec![label];
        if let Some(since) = activity.since.as_ref().filter(|_| !narrow) {
            spans.push(Span::styled(format!(" {since}"), tui_style(palette.muted)));
        }
        Some(bmux_tui::text::Line::from_spans(spans))
    }

    /// A card's second line. With the default description, what a blocked
    /// tab asks (wrapped), or its directory and git status, fitted to one
    /// line: the directory shortens to its last part, then gives way to git.
    /// Other descriptions render their template.
    fn card_detail(
        &self,
        activity: Option<&activity::TabActivity>,
        render: &dyn Fn(&str) -> String,
        room: usize,
        palette: &look::Palette,
    ) -> view::Detail {
        use bmux_plugin::render_paint::tui_style;
        use bmux_tui::text::{Line, Span};
        if self.compact() {
            return view::Detail::None;
        }
        let wrapped = |text: String| {
            if text.trim().is_empty() {
                view::Detail::None
            } else {
                view::Detail::Wrapped(view::styled(text, tui_style(palette.muted)))
            }
        };
        if self.settings.description_template != DEFAULT_DESCRIPTION {
            return wrapped(render(&self.settings.description_template));
        }
        let Some(activity) = activity else {
            return wrapped(render(&self.settings.description_template));
        };
        if let Some(message) = &activity.message {
            return view::Detail::Wrapped(view::styled(message.clone(), tui_style(palette.text)));
        }
        let mut git = Vec::new();
        if let Some(status) = &activity.git {
            git.push(Span::styled(
                status.branch.clone(),
                tui_style(palette.muted),
            ));
            if status.dirty {
                git.push(Span::styled(
                    "*",
                    tui_style(RenderStyle::new().named_foreground(RenderNamedColor::Yellow)),
                ));
            }
            if status.ahead > 0 {
                git.push(Span::styled(
                    format!(" ↑{}", status.ahead),
                    tui_style(RenderStyle::new().named_foreground(RenderNamedColor::Green)),
                ));
            }
            if status.behind > 0 {
                git.push(Span::styled(
                    format!(" ↓{}", status.behind),
                    tui_style(RenderStyle::new().named_foreground(RenderNamedColor::Red)),
                ));
            }
        }
        let git_width = Line::from_spans(git.clone()).width();
        let separator = " · ";
        let cwd = activity.cwd.as_deref().and_then(|cwd| {
            let needed = |cwd: &str| {
                bmux_cell_geometry::display_width(cwd)
                    + if git.is_empty() {
                        0
                    } else {
                        bmux_cell_geometry::display_width(separator) + git_width
                    }
            };
            if needed(cwd) <= room {
                return Some(cwd.to_string());
            }
            let last = cwd.rsplit('/').find(|part| !part.is_empty()).unwrap_or(cwd);
            if needed(last) <= room {
                return Some(last.to_string());
            }
            // Without git, the directory keeps its end, cut from the left.
            git.is_empty().then(|| activity::shorten_left(cwd, room))
        });
        let mut spare = room.saturating_sub(git_width);
        if let Some(cwd) = &cwd {
            spare = spare.saturating_sub(bmux_cell_geometry::display_width(cwd));
            if !git.is_empty() {
                spare = spare.saturating_sub(bmux_cell_geometry::display_width(separator));
            }
        }
        let branch_icon_width = bmux_cell_geometry::display_width(BRANCH_ICON);
        let icons = self.settings.icons;
        let show_branch_icon = icons && !git.is_empty() && spare >= branch_icon_width;
        if show_branch_icon {
            spare -= branch_icon_width;
        }

        let mut spans = Vec::new();
        if let Some(cwd) = cwd {
            if icons && spare >= bmux_cell_geometry::display_width(FOLDER_ICON) {
                spans.push(Span::styled(FOLDER_ICON, tui_style(palette.muted)));
            }
            spans.push(Span::styled(cwd, tui_style(palette.muted)));
            if !git.is_empty() {
                spans.push(Span::styled(separator, tui_style(palette.faint)));
            }
        }
        if show_branch_icon {
            spans.push(Span::styled(BRANCH_ICON, tui_style(palette.muted)));
        }
        spans.extend(git);
        if spans.is_empty() {
            view::Detail::None
        } else {
            view::Detail::Line(Line::from_spans(spans))
        }
    }

    /// A workspace's header: a band in the workspace's color with its name,
    /// and at the right what its tabs are doing, most pressing first (or how
    /// many it holds), then whether it is collapsed. A collapsed header that
    /// hides the shown tab has its marker in the accent color.
    fn header(
        &self,
        name: &str,
        tabs: &[usize],
        collapsed: bool,
        hovered: bool,
        palette: &look::Palette,
    ) -> view::HeaderRow {
        use bmux_plugin::render_paint::tui_style;
        use bmux_tui::text::Span;
        let holds_active = collapsed && tabs.iter().any(|index| self.snapshot.tabs[*index].active);
        let marker = if collapsed { "▸" } else { "▾" };
        // The name keeps at least a few cells; the counts take the rest, the
        // least pressing dropped first when they do not fit.
        let title_room = bmux_cell_geometry::display_width(name).min(8);
        let room =
            usize::from(self.allocated_width().saturating_sub(4)).saturating_sub(title_room + 3);
        let mut spans =
            summary_line(group_summary(self, tabs.iter().copied(), room), palette).spans;
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            marker,
            tui_style(if holds_active {
                RenderStyle::new().foreground(palette.accent).bold()
            } else {
                palette.muted
            }),
        ));
        let color = look::workspace_color(name, &self.settings.workspace_colors);
        view::HeaderRow {
            title: view::styled(name.to_string(), tui_style(palette.strong)),
            trailing: bmux_tui::text::Line::from_spans(spans),
            band: tui_style(if hovered {
                palette.band_hover
            } else {
                palette.band
            }),
            swatch: Some(tui_style(RenderStyle::new().foreground(color))),
            spacer: false,
            base: tui_style(palette.base),
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
        if self.is_strip() {
            return;
        }
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

    /// The row showing this client's tab.
    fn active_item(&self) -> Option<SidebarHit> {
        self.active_row().map(|row| self.item_keys()[row])
    }

    /// The listed rows: every tab in scope, under its workspace's header when
    /// grouped, with a collapsed workspace's tabs left out. Made once per tab
    /// list, group choice and status snapshot, and shared until one changes.
    fn rows(&self) -> std::sync::Arc<[SidebarRow]> {
        let key = (self.rows_epoch, self.activity.generation());
        if let Some((made_for, rows)) = self.rows_cache.borrow().as_ref()
            && *made_for == key
        {
            return rows.clone();
        }
        let rows: std::sync::Arc<[SidebarRow]> = self.make_rows().into();
        *self.rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    /// Forget the rows, after the tab list or a group choice changed.
    const fn rows_changed(&mut self) {
        self.rows_epoch = self.rows_epoch.wrapping_add(1);
    }

    /// The tabs in scope, with their indices in the snapshot.
    fn listed_tabs(&self) -> impl Iterator<Item = (usize, &tabs_list::TabListEntry)> + '_ {
        let current = self.focused_workspace();
        self.snapshot
            .tabs
            .iter()
            .enumerate()
            .filter(move |(_, tab)| {
                self.settings.scope == Scope::AllWorkspaces
                    || current.is_none_or(|current| tab.workspace_id == current)
            })
    }

    fn make_rows(&self) -> Vec<SidebarRow> {
        let tabs = &self.snapshot.tabs;
        let current = self.focused_workspace();
        if self.settings.grouping == Grouping::None {
            return self
                .listed_tabs()
                .map(|(index, _)| SidebarRow::Tab(index))
                .collect();
        }
        // The catalog lists tabs in workspace order; keep a workspace's tabs
        // together under the first place it appears.
        let mut groups: Vec<(Uuid, String, Vec<usize>)> = Vec::new();
        for (index, tab) in self.listed_tabs() {
            match groups.iter_mut().find(|(id, ..)| *id == tab.workspace_id) {
                Some((.., members)) => members.push(index),
                None => groups.push((tab.workspace_id, tab.workspace.clone(), vec![index])),
            }
        }
        let mut rows = Vec::new();
        for (id, name, members) in groups {
            let background = current.is_some_and(|current| current != id);
            let collapsed = self.collapsed(id, &name, members.iter().copied());
            let shown = (!collapsed).then(|| {
                members
                    .iter()
                    .copied()
                    .filter(|index| {
                        !background
                            || self.settings.other_workspace_tabs == OtherWorkspaceTabs::All
                            || self.shows_activity(tabs[*index].id)
                    })
                    .map(SidebarRow::Tab)
                    .collect::<Vec<_>>()
            });
            rows.push(SidebarRow::Group {
                id,
                name,
                tabs: members,
                collapsed,
            });
            rows.extend(shown.into_iter().flatten());
        }
        rows
    }

    /// Whether `workspace`'s group, named `name` with the tabs `members`, is
    /// collapsed: as chosen by hand, otherwise by `workspace_visibility`.
    /// The focused workspace and pinned ones stay open; following attention,
    /// so does one with a tab that needs a look (needs input, failed or done).
    fn collapsed(
        &self,
        workspace: Uuid,
        name: &str,
        mut members: impl Iterator<Item = usize>,
    ) -> bool {
        if let Some(collapsed) = self.group_overrides.get(&workspace) {
            return *collapsed;
        }
        let background = self
            .focused_workspace()
            .is_some_and(|current| current != workspace);
        background
            && !self.settings.pinned_workspaces.contains(name)
            && match self.settings.workspace_visibility {
                WorkspaceVisibility::Manual => false,
                WorkspaceVisibility::FollowFocus => true,
                WorkspaceVisibility::FollowFocusAndAttention => !members.any(|index| {
                    self.activity
                        .state(self.snapshot.tabs[index].id)
                        .is_some_and(activity::TabState::deserves_a_look)
                }),
            }
    }

    /// Whether a background tab is listed with `other_workspace_tabs =
    /// "active"`: it runs a foreground command, or its state is not at rest
    /// (working, or deserving a look).
    fn shows_activity(&self, tab: Uuid) -> bool {
        self.activity
            .status_summary(tab)
            .is_some_and(|(state, shell_job)| {
                shell_job || state == activity::TabState::Working || state.deserves_a_look()
            })
    }

    fn focused_workspace(&self) -> Option<Uuid> {
        self.snapshot
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.workspace_id)
    }

    /// Whether `workspace`'s group is collapsed; none when it has no header.
    fn group_collapsed(&self, workspace: Uuid) -> Option<bool> {
        if self.settings.grouping == Grouping::None {
            return None;
        }
        let mut members = self
            .listed_tabs()
            .filter(|(_, tab)| tab.workspace_id == workspace)
            .peekable();
        let name = members.peek()?.1.workspace.clone();
        Some(self.collapsed(workspace, &name, members.map(|(index, _)| index)))
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

    /// Whether a working spinner is on screen: the sidebar is shown, uses the
    /// spinner, and lists a working tab's card. A working tab in another
    /// workspace's scope or under a collapsed header shows no spinner.
    fn animates(&self) -> bool {
        self.visible()
            && !self.is_strip()
            && self.settings.working_indicator == WorkingIndicator::Spinner
            && self.rows().iter().any(|row| {
                matches!(row, SidebarRow::Tab(index)
                    if self.activity.state(self.snapshot.tabs[*index].id)
                        == Some(activity::TabState::Working))
            })
    }

    /// Take the runtime's appearance (the theme) for everything the sidebar
    /// paints.
    fn apply_appearance(&mut self, appearance: &bmux_appearance::RuntimeAppearance) {
        self.edge_colors = EdgeColors::from_appearance(appearance);
        self.palette = look::Palette::from_appearance(appearance);
    }

    /// Collapse or expand `workspace`'s group and remember it. Returns a
    /// description of a failure to remember it.
    fn toggle_group(&mut self, workspace: Uuid) -> Option<String> {
        let collapsed = self.group_collapsed(workspace)?;
        self.group_overrides.insert(workspace, !collapsed);
        self.rows_changed();
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
            .group_overrides
            .iter()
            .map(|(id, collapsed)| {
                if *collapsed {
                    id.to_string()
                } else {
                    format!("+{id}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let write = || -> std::io::Result<()> {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = file.with_extension(format!("{}.tmp", Uuid::new_v4()));
            std::fs::write(&temporary, text)?;
            std::fs::rename(&temporary, file)
        };
        write()
            .map_err(|error| format!("remembering sidebar groups in {}: {error}", file.display()))
    }

    /// Apply a catalog snapshot, arranged by this attachment's personal view
    /// when it has one ([`personal_tabs::arrange`]). The shared catalog cannot
    /// know which tab is this attachment's, so a known client-local selection
    /// decides `active`, exactly as the tab bar does.
    fn replace_tabs(&mut self, catalog: tabs_list::TabListSnapshot) {
        let snapshot =
            personal_tabs::arrange(&catalog, self.personal.as_ref(), self.selected_context_id);
        self.catalog = catalog;
        if self.snapshot != snapshot {
            let active = |snapshot: &tabs_list::TabListSnapshot| {
                snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.active)
                    .map(|tab| tab.id)
            };
            let switched = active(&self.snapshot) != active(&snapshot);
            self.snapshot = snapshot;
            self.rows_changed();
            let follows_focus = self.settings.grouping == Grouping::Workspace
                && self.settings.workspace_visibility != WorkspaceVisibility::Manual;
            if let Some(workspace) = self.focused_workspace() {
                let previous = self.last_focused_workspace.replace(workspace);
                if follows_focus
                    && previous.is_some_and(|previous| previous != workspace)
                    && !self.group_overrides.is_empty()
                {
                    self.group_overrides.clear();
                    self.rows_changed();
                    if let Err(error) = self.remember_groups() {
                        tracing::warn!(%error, "clearing sidebar group overrides");
                    }
                }
            }
            let keys = self.item_keys();
            self.cursor.retain(&keys);
            // Switching tabs always brings the shown one into view. Other
            // catalog changes keep the scroll where groups and rows come and
            // go with focus or activity, so they do not yank it.
            let filters_tabs = self.settings.grouping == Grouping::Workspace
                && self.settings.other_workspace_tabs == OtherWorkspaceTabs::Active;
            self.settle_scroll(switched || (!follows_focus && !filters_tabs));
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

    /// Adopt this attachment's personal view projection (the nil view means
    /// it has none). Returns whether it changed.
    fn set_personal(&mut self, projection: personal_view::PersonalViewProjection) -> bool {
        let projection = (!projection.view_id.is_nil()).then_some(projection);
        if self.personal == projection {
            return false;
        }
        self.personal = projection;
        true
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
        let (initial_personal, mut personal_rx) = subscribe_personal_view(&resources.events)?;
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
                companion.apply_appearance(appearance);
            }
            companion.selected_context_id = initial_selection;
            companion.set_personal(initial_personal.as_ref().clone());
            companion.replace_tabs(initial.as_ref().clone());
            publish_companion(companion)?;
            let owner = std::sync::Arc::downgrade(&presentation.owner);
            companion.subscription = Some(handle.spawn(async move {
                // Ages and working/done states change with time alone.
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // The working spinner's frames, only while one can be on screen.
                let mut frame = spinner_frames();
                loop {
                    let animating = animating(&owner);
                    let mut changes = Changes::default();
                    tokio::select! {
                        result = tabs_rx.changed() => {
                            if result.is_err() { break; }
                            changes.tabs = Some(tabs_rx.borrow_and_update().as_ref().clone());
                        }
                        result = personal_rx.changed() => {
                            if result.is_err() { break; }
                            changes.personal =
                                Some(personal_rx.borrow_and_update().as_ref().clone());
                        }
                        context_id = next_selection(&mut selection_rx) => {
                            changes.selection = Some(SelectionChange(context_id));
                        }
                        result = facts_rx.changed() => {
                            if result.is_err() { break; }
                            facts_rx.borrow_and_update();
                        }
                        result = status_rx.changed() => {
                            if result.is_err() { break; }
                            changes.status = Some(status_rx.borrow_and_update().as_ref().clone());
                        }
                        appearance = next_appearance(&mut appearance_rx) => {
                            changes.appearance = Some(appearance);
                        }
                        _ = tick.tick() => changes.ticked = true,
                        _ = frame.tick(), if animating => changes.animated = true,
                    }
                    if !apply_and_publish(&owner, changes) {
                        break;
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
    // From a strip (minimized, or the host's below `collapse_below_width`),
    // widening starts from the remembered expanded width, not the strip's.
    let responsive_strip = companion.visible()
        && companion.is_strip()
        && companion.width_mode == WidthMode::Responsive;
    let width = if widen && (responsive_strip || companion.width_mode == WidthMode::Collapsed) {
        companion.width
    } else if responsive_strip {
        companion.allocated_width()
    } else {
        companion.requested_width()
    };
    let width = if widen {
        width.saturating_add(WIDTH_STEP)
    } else {
        width.saturating_sub(WIDTH_STEP)
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
            companion.engaged = Engagement::default();
            companion.resize = None;
            companion.width_mode = WidthMode::Responsive;
            companion.expand_if_strip = false;
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
        release_capture: changed && !companion.visible(),
        ..AttachInputResult::default()
    }
}

fn layout_request(
    settings: &Settings,
    width: u16,
    visible: bool,
    mode: WidthMode,
) -> PluginLayoutRequest {
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
        match mode {
            WidthMode::Collapsed => LayoutExtent::Cells(settings.collapsed_width),
            // Expanded on a narrow terminal: never so wide the panes have no
            // room. (Above the breakpoint, the responsive width leaves them
            // at least `collapse_below_width - width`.)
            WidthMode::Expanded => LayoutExtent::Reserving {
                preferred: width,
                minimum: settings.collapsed_width,
                reserve: PANE_RESERVE,
            },
            WidthMode::Responsive => LayoutExtent::Responsive {
                preferred: width,
                collapsed: settings.collapsed_width,
                collapse_below: settings.collapse_below_width,
            },
        },
    )
}

/// Publish the layout request for the companion's current visibility.
fn publish_layout(companion: &CompanionState) -> Result<(), String> {
    let Some(layouts) = companion.layouts.as_deref() else {
        return Ok(());
    };
    let request = layout_request(
        &companion.settings,
        companion.width,
        companion.visible(),
        companion.width_mode,
    );
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

/// The tab a template describes, and how.
#[derive(Clone, Copy)]
struct TemplateTab<'a> {
    tab: &'a tabs_list::TabListEntry,
    /// Its position in the tab list.
    index: usize,
    show_index: bool,
    /// Whether `{folder_icon}` and `{branch_icon}` expand to Nerd Font icons.
    icons: bool,
}

fn render_template(
    template: &str,
    tab: TemplateTab<'_>,
    fact: Option<&PresentationFact>,
    activity: Option<&activity::TabActivity>,
) -> String {
    const MARKER_TOKEN: &str = concat!("{", "marker}");
    const INDEX_TOKEN: &str = concat!("{", "index}");
    const FACT_TOKEN: &str = concat!("{", "fact}");
    const FOLDER_ICON_TOKEN: &str = concat!("{", "folder_icon}");
    const BRANCH_ICON_TOKEN: &str = concat!("{", "branch_icon}");
    let TemplateTab {
        tab,
        index,
        show_index,
        icons,
    } = tab;
    let marker = if tab.active { "●" } else { "○" };
    let index = if show_index {
        format!("{} ", index.saturating_add(1))
    } else {
        String::new()
    };
    // An icon shows only beside what it marks.
    let icon = |icon: &'static str, marks: Option<&String>| {
        if icons && marks.is_some() { icon } else { "" }
    };
    let folder_icon = icon(
        FOLDER_ICON,
        activity.and_then(|activity| activity.cwd.as_ref()),
    );
    let branch_icon = icon(
        BRANCH_ICON,
        activity.and_then(|activity| activity.branch.as_ref()),
    );
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
        .replace(FOLDER_ICON_TOKEN, folder_icon)
        .replace(BRANCH_ICON_TOKEN, branch_icon)
        .replace('\u{0}', "{")
        .replace('\u{1}', "}")
}

/// A tab's state glyph and color. They show only what the tab is doing; the
/// card's tint and accent bar show which tab is selected.
fn state_glyph(activity: Option<&activity::TabActivity>) -> (char, RenderColor) {
    let state = activity.map_or(activity::TabState::Idle, |activity| activity.state);
    (state.glyph(), state.color())
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
    if width < 2 {
        return None;
    }
    let height = if state.settings.content_height && !state.is_strip() {
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

/// The heading's right side: what every listed tab is doing, most pressing
/// first, or how many tabs there are when none is doing anything, in the room
/// the heading and the window controls (`controls` cells) leave.
fn sidebar_summary(
    state: &CompanionState,
    controls: u16,
    palette: &look::Palette,
) -> bmux_tui::text::Line {
    // A grouped list's headers hold every listed tab, shown or collapsed;
    // an ungrouped list's tab rows do.
    let rows = state.rows();
    let grouped = rows
        .iter()
        .any(|row| matches!(row, SidebarRow::Group { .. }));
    let tabs = rows.iter().flat_map(|row| match row {
        SidebarRow::Group { tabs, .. } => tabs.as_slice(),
        SidebarRow::Tab(index) if !grouped => std::slice::from_ref(index),
        SidebarRow::Tab(_) => &[],
    });
    // A column before the heading, a blank cell after it and before the
    // controls, and the edge column.
    let chrome = 4;
    let room = usize::from(state.allocated_width()).saturating_sub(
        bmux_cell_geometry::display_width(&state.settings.heading) + usize::from(controls) + chrome,
    );
    summary_line(group_summary(state, tabs.copied(), room), palette)
}

/// Summary parts as one line: counts bold in their states' colors, a plain
/// tab count muted.
fn summary_line(
    parts: Vec<(String, Option<RenderColor>)>,
    palette: &look::Palette,
) -> bmux_tui::text::Line {
    use bmux_plugin::render_paint::{tui_color, tui_style};
    use bmux_tui::text::Span;
    let mut spans = Vec::with_capacity(parts.len() * 2);
    for (index, (text, color)) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            text,
            color.map_or_else(
                || tui_style(palette.muted),
                |color| {
                    bmux_tui::style::Style::new()
                        .fg(tui_color(color))
                        .add_modifier(bmux_tui::style::Modifier::BOLD)
                },
            ),
        ));
    }
    bmux_tui::text::Line::from_spans(spans)
}

/// What a group's tabs are doing, as `◆1 ◉2 ■3`-style counts in their
/// states' colors, most pressing first, within `room` cells; the tab count
/// when none is doing anything.
fn group_summary(
    state: &CompanionState,
    tabs: impl IntoIterator<Item = usize>,
    room: usize,
) -> Vec<(String, Option<RenderColor>)> {
    let (counts, total) = status_counts(state, tabs);
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
        let count = total.to_string();
        if count.len() <= room {
            parts.push((count, None));
        }
    }
    parts
}

/// How many of `tabs` are in each state other than idle (a tab running a
/// foreground command beside its agent counts as a shell too), and how many
/// tabs there are.
fn status_counts(
    state: &CompanionState,
    tabs: impl IntoIterator<Item = usize>,
) -> (std::collections::BTreeMap<activity::TabState, usize>, usize) {
    let mut counts = std::collections::BTreeMap::<activity::TabState, usize>::new();
    let mut total = 0;
    for index in tabs {
        total += 1;
        let tab = &state.snapshot.tabs[index];
        if let Some((status, has_shell_job)) = state.activity.status_summary(tab.id) {
            if status != activity::TabState::Idle {
                *counts.entry(status).or_default() += 1;
            }
            if status != activity::TabState::Shell && has_shell_job {
                *counts.entry(activity::TabState::Shell).or_default() += 1;
            }
        }
    }
    (counts, total)
}

/// The strip's lines below its restore control: a count per state across
/// every tab (just the glyph where the count does not fit), or the number of
/// tabs when none is doing anything.
fn strip_summary(state: &CompanionState) -> Vec<bmux_tui::text::Line> {
    use bmux_plugin::render_paint::{tui_color, tui_style};
    use bmux_tui::style::{Modifier, Style};

    let room = usize::from(state.allocated_width().saturating_sub(1).max(1));
    let (counts, total) = status_counts(state, 0..state.snapshot.tabs.len());
    if counts.is_empty() {
        let label = total.to_string();
        return if label.len() <= room {
            vec![view::styled(label, tui_style(state.palette.muted))]
        } else {
            Vec::new()
        };
    }
    counts
        .into_iter()
        .map(|(status, count)| {
            let label = format!("{}{count}", status.glyph());
            view::styled(
                if bmux_cell_geometry::display_width(&label) <= room {
                    label
                } else {
                    status.glyph().to_string()
                },
                Style::new()
                    .fg(tui_color(status.color()))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
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
        } else if hit.local_id.starts_with(view::WINDOW_ID) {
            region_item(&hit.local_id)
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
            SidebarHit::Tab(_) | SidebarHit::Group(_) | SidebarHit::Control(_) => {
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
            SidebarHit::ResizeEdge | SidebarHit::Scrollbar | SidebarHit::Control(_) => region,
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
fn sidebar_view<'a>(
    state: &'a CompanionState,
    rows: &'a [(SidebarHit, view::RowView)],
) -> view::SidebarView<'a, SidebarHit> {
    use bmux_plugin::render_paint::tui_style;
    let width = state.allocated_width();
    let palette = state.palette;
    let strip = state.is_strip();
    let keys = rows.iter().map(|(key, _)| *key).collect::<Vec<_>>();
    let list = if strip {
        view::list(rows)
    } else {
        state.synced_list(rows)
    };
    let list_height = if strip { 0 } else { state.list_height(&keys) };
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
    let controls = WindowControls::new(if strip {
        PanelShape::Strip
    } else {
        PanelShape::Normal
    })
    .maximizable(false)
    .style(tui_style(palette.muted.background_of(palette.base)))
    .hovered(state.engaged.control_hovered);
    let strip_lines = strip.then(|| strip_summary(state));
    let height = match &strip_lines {
        // Unallocated, a strip is as tall as its control and summary.
        Some(lines) => state.allocation.map_or_else(
            || {
                u16::try_from(lines.len())
                    .unwrap_or(u16::MAX)
                    .saturating_add(1)
            },
            |rect| rect.h,
        ),
        None if state.settings.content_height => shown_rows.saturating_add(2),
        None => state
            .allocation
            .map_or_else(|| list_height.saturating_add(2), |rect| rect.h),
    }
    .max(1);
    let summary = if strip {
        bmux_tui::text::Line::default()
    } else {
        sidebar_summary(state, controls.inline_width(), &palette)
    };
    let highlighted = state.engaged.edge_hovered || state.resize.is_some();
    let edge_color = if highlighted {
        state.edge_colors.hover
    } else {
        state.edge_colors.track
    };
    let edge = resize_edge(state, shown_rows).map(|edge| view::Edge {
        x: edge.x,
        height: edge.h.min(height),
        highlighted,
        style: tui_style(edge_color.background_of(palette.base)),
    });
    view::SidebarView {
        size: (width, height),
        base: tui_style(palette.base),
        heading: view::styled(
            state.settings.heading.to_uppercase(),
            // Lit while the sidebar has the keyboard.
            tui_style(if state.engaged.focused {
                RenderStyle::new()
                    .foreground(palette.accent)
                    .bold()
                    .underline()
            } else {
                palette.muted.bold()
            }),
        ),
        count: summary,
        controls,
        strip_summary: strip_lines,
        list,
        state: &state.list,
        list_height,
        list_columns,
        scrollbar: scrollbar_x
            .filter(|_| !strip)
            .map(|x| scrollbar_view(state, x, list_height, &palette)),
        edge,
    }
}

fn build_surface_with_hits(
    state: &CompanionState,
    revision: u64,
) -> (PluginSurface, HitTable<SidebarHit>) {
    let mut hits = HitTable::new();
    let rows = if state.is_strip() {
        Vec::new()
    } else {
        state.row_views(revision)
    };
    let component = sidebar_view(state, &rows);
    let (width, height) = component.size;
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
    // Fields are laid out at the list's width, one column short of the
    // sidebar's while the scrollbar shows.
    let text = visible_field_text(&rows, &placed, component.list_columns.1);
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
#[derive(Default)]
struct Changes {
    tabs: Option<tabs_list::TabListSnapshot>,
    /// A new personal view projection for this attachment.
    personal: Option<personal_view::PersonalViewProjection>,
    selection: Option<SelectionChange>,
    status: Option<tabs_status::TabStatusSnapshot>,
    appearance: Option<bmux_appearance::RuntimeAppearance>,
    ticked: bool,
    /// The working spinner's next frame is due.
    animated: bool,
}

impl CompanionState {
    /// Take `changes`; returns whether the sidebar must be republished.
    fn apply_changes(&mut self, changes: Changes) -> bool {
        let Changes {
            tabs,
            personal,
            selection,
            status,
            appearance,
            ticked,
            animated,
        } = changes;
        if let Some(appearance) = &appearance {
            self.apply_appearance(appearance);
        }
        if ticked && !self.activity.time_sensitive() {
            return false;
        }
        if animated
            && tabs.is_none()
            && personal.is_none()
            && selection.is_none()
            && status.is_none()
        {
            // Only the spinner's glyph changes; nothing needs measuring.
            if !self.animates() {
                return false;
            }
            self.presentation_pending.set(true);
            return true;
        }
        let status_changed = status.is_some();
        if let Some(status) = status {
            self.activity.replace(status);
        }
        let personal_changed = personal.is_some_and(|projection| self.set_personal(projection));
        if status_changed
            && self.settings.grouping == Grouping::Workspace
            && (self.settings.workspace_visibility == WorkspaceVisibility::FollowFocusAndAttention
                || self.settings.other_workspace_tabs == OtherWorkspaceTabs::Active)
        {
            let keys = self.item_keys();
            self.cursor.retain(&keys);
        }
        if let Some(tabs) = tabs {
            if let Some(SelectionChange(context_id)) = selection {
                self.selected_context_id = context_id;
            }
            self.replace_tabs(tabs);
        } else if let Some(SelectionChange(context_id)) = selection {
            self.select(context_id);
            if personal_changed {
                self.replace_tabs(self.catalog.clone());
            }
        } else if personal_changed {
            self.replace_tabs(self.catalog.clone());
        } else {
            // Only a fact change brings the active tab back into
            // view; status updates and ticks keep the scroll.
            self.remeasure(!ticked && !status_changed);
        }
        self.presentation_pending.set(true);
        true
    }
}

/// Apply `changes` to the sidebar `owner` holds and republish it when they
/// change what it shows. Returns `false` once the sidebar is gone.
fn apply_and_publish(
    owner: &std::sync::Weak<Mutex<Option<CompanionState>>>,
    changes: Changes,
) -> bool {
    let Some(owner) = owner.upgrade() else {
        return false;
    };
    let Ok(mut guard) = owner.lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    if companion.apply_changes(changes)
        && let Err(error) = publish_companion(companion)
    {
        tracing::warn!(%error, "sidebar publication failed");
    }
    true
}

/// Ticks for the working spinner's frames.
fn spinner_frames() -> tokio::time::Interval {
    let mut frames =
        tokio::time::interval(std::time::Duration::from_millis(look::SPINNER_FRAME_MS));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    frames
}

/// Whether the sidebar `owner` holds shows a working spinner.
fn animating(owner: &std::sync::Weak<Mutex<Option<CompanionState>>>) -> bool {
    owner.upgrade().is_some_and(|owner| {
        owner
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(CompanionState::animates))
            .unwrap_or(false)
    })
}

/// The receiver of this attachment's client-local selection.
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

/// Follow the tabs plugin's forwarded `tabs-status` state on this runtime's
/// bus, registering its decoder when nothing has yet.
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

/// This attachment's personal tab view projection, delivered only to it by
/// the tabs plugin through the runtime's generic forwarded-state path;
/// registers its decoder when nothing has yet (the tab bar may have). The nil
/// view means none.
fn subscribe_personal_view(
    events: &std::sync::Arc<bmux_plugin::EventBus>,
) -> Result<
    (
        std::sync::Arc<personal_view::PersonalViewProjection>,
        tokio::sync::watch::Receiver<std::sync::Arc<personal_view::PersonalViewProjection>>,
    ),
    String,
> {
    let kind = &personal_view::STATE_KIND;
    if let Ok(subscription) = events.subscribe_state::<personal_view::PersonalViewProjection>(kind)
    {
        return Ok(subscription);
    }
    events.register_state_channel_with_decoder(
        kind.clone(),
        personal_view::PersonalViewProjection {
            last_removed: None,
            following: None,
            is_default: true,
            view_id: Uuid::nil(),
            name: String::new(),
            view_revision: 0,
            binding_revision: 0,
            active_workspace_id: Uuid::nil(),
            workspaces: Vec::new(),
        },
    );
    events
        .subscribe_state::<personal_view::PersonalViewProjection>(kind)
        .map_err(|error| format!("subscribing to the personal tab view: {error}"))
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
    _palette: &look::Palette,
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
                start_width: companion.allocated_width(),
                restore_width: companion.width,
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
            if !companion.set_width(width) {
                return false;
            }
            if matches!(companion.width_mode, WidthMode::Collapsed) {
                companion.width = drag.restore_width;
            }
            republish_width(companion, true)
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
    let previous_engagement = companion.engaged;
    // The column facing the panes lights up anywhere on it, thumb included.
    companion.engaged.edge_hovered = matches!(
        hovered,
        Some(SidebarHit::ResizeEdge | SidebarHit::Scrollbar)
    );
    companion.engaged.control_hovered = match hovered {
        Some(SidebarHit::Control(control)) => Some(control),
        _ => None,
    };
    let row = hovered.filter(|hit| matches!(hit, SidebarHit::Tab(_) | SidebarHit::Group(_)));
    if !companion.cursor.set_hovered(row) && previous_engagement == companion.engaged {
        return false;
    }
    companion.revision = companion.revision.saturating_add(1).max(1);
    if publish_companion(companion).is_err() {
        companion.cursor = previous;
        companion.engaged = previous_engagement;
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
    if companion.is_strip() {
        return false;
    }
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
    if matches!(target, SidebarHit::Control(_)) && event.key.as_deref() != Some("esc") {
        return None;
    }
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
            let collapsed = companion.group_collapsed(workspace)?;
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
        SidebarHit::Control(WindowControl::Minimize) => {
            if companion.is_strip() {
                companion.restore();
            } else {
                companion.width_mode = WidthMode::Collapsed;
            }
            Some(AttachInputResult {
                consumed: true,
                preserve_focus: true,
                dirty: republish_width(companion, true),
                ..AttachInputResult::default()
            })
        }
        SidebarHit::Control(WindowControl::Close) => {
            Some(apply_visibility(companion, VisibilityCommand::Hide))
        }
        SidebarHit::Control(WindowControl::Maximize)
        | SidebarHit::ResizeEdge
        | SidebarHit::Scrollbar => None,
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

    /// A tab doing nothing, with no signals.
    fn idle_status() -> bmux_tabs_plugin_api::tabs_status::TabStatus {
        bmux_tabs_plugin_api::tabs_status::TabStatus {
            tab_id: Uuid::nil(),
            state: bmux_tabs_plugin_api::tabs_status::TabState::Idle,
            state_since_ms: None,
            panes: 1,
            shell_command: None,
            running_command: None,
            cwd: None,
            title: None,
            git: None,
            last_exit: None,
            output_at_ms: None,
            attention_at_ms: None,
            attention: None,
            agent: None,
        }
    }

    /// Shown, with the still working glyph: a spinner's frame depends on the
    /// time a test runs.
    fn shown_settings() -> Settings {
        Settings {
            visibility: VisibilitySettings {
                visible: true,
                ..VisibilitySettings::default()
            },
            working_indicator: WorkingIndicator::Static,
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
        let left = layout_request(&shown_settings(), 28, true, WidthMode::Responsive);
        let right = layout_request(
            &Settings {
                placement: Placement::Right,
                order: 50,
                ..shown_settings()
            },
            28,
            true,
            WidthMode::Responsive,
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
            render_template(
                concat!("{", "fact}"),
                TemplateTab {
                    tab: &tab,
                    index: 0,
                    show_index: false,
                    icons: true,
                },
                fact.as_ref(),
                None
            ),
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
        let settings: toml::Value =
            toml::from_str("placement = 'right'\nwidth = 24\nmaximum_width = 40").unwrap();
        let settings = Settings::parse(Some(&settings)).unwrap();
        assert_eq!(settings.placement, Placement::Right);
        assert_eq!(settings.title_template, "{state_icon} {name}");
        assert_eq!(settings.collapse_below_width, 80);
        let invalid: toml::Value =
            toml::from_str("width = 10\ncollapsed_width = 12\nmaximum_width = 40").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
        // A strip as wide as the expanded sidebar could not be told from it.
        let equal: toml::Value = toml::from_str("width = 12\ncollapsed_width = 12").unwrap();
        assert!(Settings::parse(Some(&equal)).is_err());
        // The removed `minimum_width` is ignored (with a warning).
        let removed: toml::Value = toml::from_str("width = 24\nminimum_width = 12").unwrap();
        assert_eq!(Settings::parse(Some(&removed)).unwrap().width, 24);
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
                TemplateTab {
                    tab: &tab,
                    index: 1,
                    show_index: true,
                    icons: true,
                },
                None,
                None
            ),
            "{literal} 2 build active"
        );
    }

    /// `{folder_icon}` and `{branch_icon}` are the Nerd Font icons beside a
    /// directory and branch the tab has, and nothing with `icons = false`.
    #[test]
    fn icon_placeholders_follow_the_icons_setting() {
        let tab = tabs_list::TabListEntry {
            id: Uuid::from_u128(10),
            name: "build".to_string(),
            active: false,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        let activity = activity::TabActivity {
            state: activity::TabState::Idle,
            summary: None,
            detail: String::new(),
            command: None,
            cwd: Some("~/bmux".into()),
            branch: None,
            agent: None,
            git: None,
            message: None,
            since: None,
        };
        let render = |icons| {
            render_template(
                "{folder_icon}{cwd} {branch_icon}{branch}",
                TemplateTab {
                    tab: &tab,
                    index: 0,
                    show_index: false,
                    icons,
                },
                None,
                Some(&activity),
            )
        };
        assert_eq!(render(true), format!("{FOLDER_ICON}~/bmux "));
        assert_eq!(render(false), "~/bmux ");
        let off = toml::Value::Table(toml::toml! { icons = false });
        assert!(!Settings::parse(Some(&off)).unwrap().icons);
        assert!(Settings::default().icons);
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
                TemplateTab {
                    tab: &tab,
                    index: 0,
                    show_index: true,
                    icons: true,
                },
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
    async fn forwarded_personal_view_decides_the_listed_tabs_and_their_order() {
        let resources = bmux_plugin::AttachPresentationResources::isolated(4);
        let [one, two, three] = [31, 32, 33].map(Uuid::from_u128);
        let entry = |id| tabs_list::TabListEntry {
            id,
            name: id.to_string(),
            active: false,
            workspace: "default".to_string(),
            workspace_id: Uuid::nil(),
        };
        resources.events.register_state_channel(
            tabs_list::STATE_KIND,
            tabs_list::TabListSnapshot {
                tabs: vec![entry(one), entry(two), entry(three)],
                revision: 1,
            },
        );
        let presentation = SidebarPresentation::install_for_runtime(None, &resources).unwrap();
        let listed = |presentation: &SidebarPresentation| {
            presentation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .snapshot
                .tabs
                .iter()
                .map(|tab| tab.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(&presentation).len(), 3, "the catalog without a view");
        // The runtime's generic fallback for forwarded plugin-bus payloads.
        let member = |id: Uuid, name: &str| personal_view::PersonalViewMember {
            member_id: Uuid::new_v4(),
            context_id: id,
            name: name.to_string(),
            label: None,
            active: false,
        };
        let payload = serde_json::to_vec(&personal_view::PersonalViewProjection {
            last_removed: None,
            following: None,
            is_default: true,
            view_id: Uuid::from_u128(0xF1),
            name: "default".into(),
            view_revision: 2,
            binding_revision: 1,
            active_workspace_id: Uuid::nil(),
            workspaces: vec![personal_view::PersonalViewWorkspace {
                workspace_id: Uuid::nil(),
                members: vec![member(three, "last-first"), member(one, "one")],
            }],
        })
        .unwrap();
        assert!(
            resources
                .events
                .emit_from_bytes(&personal_view::STATE_KIND, &payload)
                .unwrap()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while listed(&presentation) != ["last-first", "one"] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
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
    /// working, needing input and stopped, and `notes` (tab 4) idle, each in
    /// the state the tabs plugin publishes for it.
    fn grouped_state(settings: Settings, state_dir: Option<std::path::PathBuf>) -> CompanionState {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, TabAgent, TabState, TabStatus, TabStatusSnapshot,
        };
        let mut state = CompanionState::new(settings, state_dir);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 14));
        let now = 600_000;
        let status = |index: u128, state: TabState, activity: Option<AgentActivity>| {
            let finished_at_ms = (activity == Some(AgentActivity::Idle)).then_some(now - 5_000);
            TabStatus {
                tab_id: Uuid::from_u128(index),
                state,
                state_since_ms: activity.map(|_| now - 5_000),
                panes: 1,
                shell_command: None,
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
                    finished_at_ms,
                }),
            }
        };
        state.activity.replace(TabStatusSnapshot {
            tabs: vec![
                status(1, TabState::Working, Some(AgentActivity::Working)),
                status(2, TabState::NeedsInput, Some(AgentActivity::Blocked)),
                status(3, TabState::Stopped, Some(AgentActivity::Idle)),
                status(4, TabState::Idle, None),
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

    /// Grouped, each workspace's tabs sit under its header, a band set off
    /// from the group above by a blank row, which says what they are doing:
    /// how many need input, are working and have stopped. A rail in the
    /// workspace's color runs down its tabs.
    #[test]
    fn grouped_tabs_sit_under_headers_that_sum_up_their_work() {
        let mut state = grouped_state(grouped(), None);
        // A theme tints the band; without one, nothing is tinted.
        state.apply_appearance(&bmux_appearance::RuntimeAppearance {
            foreground: "#c0caf5".into(),
            background: "#1a1b26".into(),
            ..bmux_appearance::RuntimeAppearance::default()
        });
        let surface = build_surface(&state, 1);
        let text = surface_text(&surface, 32, 8);
        assert_eq!(
            text,
            [
                " TABS              ◆1 ◉1 ■1 ▾ ×",
                "▌ bmux               ◆1 ◉1 ■1 ▾",
                "▎ ◉ build            working 5s",
                "▏ ◆ review         needs input",
                "▏ ■ docs             stopped 5s",
                "",
                "▌ notes                     1 ▾",
                "▏ ○ todo",
            ],
            "{}",
            text.join("\n")
        );
        // The band spans the row; the swatch and rails take the workspace's color.
        let band = painted_row(&surface, 1);
        assert!(
            band.iter().all(|(_, _, style)| style.bg.is_some()),
            "{band:?}"
        );
        let swatch = band
            .iter()
            .find(|(x, ..)| *x == 0)
            .map(|(.., style)| style.fg);
        let rail = painted_row(&surface, 3)
            .into_iter()
            .find(|(x, ..)| *x == 0)
            .map(|(.., style)| style.fg);
        assert_eq!(swatch, rail);
        let notes = painted_row(&surface, 6)
            .into_iter()
            .find(|(x, ..)| *x == 0)
            .map(|(.., style)| style.fg);
        assert_ne!(swatch, notes, "workspaces have their own colors");
        // Flat: no headers and no rails.
        let flat = grouped_state(shown_settings(), None);
        let text = surface_text(&build_surface(&flat, 1), 32, 6);
        assert_eq!(text[1], "▎ ◉ build            working 5s");
        assert_eq!(text[4], "  ○ todo");
    }

    /// Input is shown in a filled badge, and working uses colored text.
    #[test]
    fn attention_states_show_solid_pills() {
        let state = grouped_state(grouped(), None);
        let surface = build_surface(&state, 1);
        let text = surface_text(&surface, 32, 4);
        let row = u16::try_from(
            text.iter()
                .position(|line| line.contains("review"))
                .unwrap(),
        )
        .unwrap();
        let pill = painted_row(&surface, row)
            .into_iter()
            .find(|(_, text, _)| text.contains("needs input"))
            .expect("pill");
        assert_eq!(pill.1, " needs input ");
        assert_eq!(pill.2.bg, Some(activity::TabState::Blocked.color()));
        assert!(pill.2.bold);
        let working = painted_row(&surface, 2)
            .into_iter()
            .find(|(_, text, _)| text.contains("working"))
            .expect("working");
        assert_eq!(working.2.fg, Some(activity::TabState::Working.color()));
        assert_eq!(
            working.2.bg, state.palette.active_card.bg,
            "on the shown tab's card"
        );
    }

    /// By default a working tab's glyph spins, one frame per tick for every
    /// working tab alike; `working_indicator = "static"` keeps `◉`.
    #[test]
    fn working_tabs_spin_unless_set_still() {
        let glyph = |settings: Settings| {
            let state = grouped_state(settings, None);
            let surface = build_surface(&state, 1);
            painted_row(&surface, 2)
                .into_iter()
                .find(|(x, ..)| *x == 2)
                .map(|(_, text, _)| text.chars().next().unwrap())
                .unwrap()
        };
        let spinning = glyph(Settings {
            working_indicator: WorkingIndicator::Spinner,
            ..grouped()
        });
        assert!(('⠀'..='⣿').contains(&spinning), "{spinning:?}");
        assert_eq!(glyph(grouped()), '◉');
        let parsed = Settings::parse(Some(&toml::Value::Table(toml::toml! {
            working_indicator = "static"
        })))
        .unwrap();
        assert_eq!(parsed.working_indicator, WorkingIndicator::Static);
        assert_eq!(
            Settings::default().working_indicator,
            WorkingIndicator::Spinner
        );
        assert!(
            Settings::parse(Some(&toml::Value::Table(toml::toml! {
                working_indicator = "blink"
            })))
            .is_err()
        );
        // The spinner animates only while a working tab can be on screen.
        let mut state = grouped_state(
            Settings {
                working_indicator: WorkingIndicator::Spinner,
                ..grouped()
            },
            None,
        );
        assert!(state.animates());
        state.settings.working_indicator = WorkingIndicator::Static;
        assert!(!state.animates());
    }

    /// `density = "compact"` shows one line per tab; below the compact width
    /// cards are one line anyway, and their state is their glyph alone.
    #[test]
    fn compact_cards_are_one_line() {
        let card_rows = |state: &CompanionState| {
            surface_text(&build_surface(state, 1), state.allocated_width(), 14)
                .into_iter()
                .filter(|line| !line.trim().is_empty())
                .count()
        };
        let mut state = fixture_with_details(Settings {
            density: Density::Compact,
            ..shown_settings()
        });
        assert_eq!(card_rows(&state), 1 + 2, "the heading and one row per tab");
        state.settings.density = Density::Comfortable;
        assert_eq!(card_rows(&state), 1 + 4);
        state.allocation = Some(ExtensionRect::new(0, 0, 16, 14));
        assert_eq!(card_rows(&state), 1 + 2);
        let text = surface_text(&build_surface(&state, 1), 16, 3);
        assert_eq!(text[2], "  ◆ review", "no summary beside the name");
        let parsed = Settings::parse(Some(&toml::Value::Table(toml::toml! {
            density = "compact"
        })))
        .unwrap();
        assert_eq!(parsed.density, Density::Compact);
    }

    /// Two tabs with a second line each: one working in a repository, one
    /// asking the user something.
    fn fixture_with_details(settings: Settings) -> CompanionState {
        use bmux_tabs_plugin_api::tabs_status::{
            AgentActivity, GitStatus, TabAgent, TabState, TabStatus, TabStatusSnapshot,
        };
        let mut state = CompanionState::new(settings, None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 14));
        let now = 600_000;
        let status = |index: u128, state, activity, message: Option<&str>| TabStatus {
            tab_id: Uuid::from_u128(index),
            state,
            state_since_ms: Some(now - 5_000),
            panes: 1,
            shell_command: None,
            running_command: Some("claude".into()),
            cwd: Some("/work/bmux".into()),
            title: None,
            git: Some(GitStatus {
                branch: "main".into(),
                dirty: true,
                ahead: 0,
                behind: 0,
            }),
            last_exit: None,
            output_at_ms: None,
            attention_at_ms: None,
            attention: None,
            agent: Some(TabAgent {
                name: "claude".into(),
                activity,
                message: message.map(Into::into),
                changed_at_ms: now - 5_000,
                finished_at_ms: None,
            }),
        };
        state.activity.replace(TabStatusSnapshot {
            tabs: vec![
                status(1, TabState::Working, AgentActivity::Working, None),
                status(
                    2,
                    TabState::NeedsInput,
                    AgentActivity::Blocked,
                    Some("approve?"),
                ),
            ],
            now_ms: now,
            revision: 1,
        });
        state.replace_tabs(tabs_list::TabListSnapshot {
            tabs: [(1, "build"), (2, "review")]
                .into_iter()
                .map(|(index, name)| tabs_list::TabListEntry {
                    id: Uuid::from_u128(index),
                    name: name.into(),
                    active: false,
                    workspace: "default".into(),
                    workspace_id: Uuid::nil(),
                })
                .collect(),
            revision: 1,
        });
        state
    }

    /// The second line fits on one row: the directory gives way to its last
    /// part, then to the git status, whose parts keep their colors.
    #[test]
    fn the_detail_line_shortens_the_directory_before_git() {
        let detail = |width: u16| {
            let mut state = fixture_with_details(shown_settings());
            state.allocation = Some(ExtensionRect::new(0, 0, width, 14));
            surface_text(&build_surface(&state, 1), width, 3)[2]
                .trim()
                .to_string()
        };
        assert_eq!(detail(32), "\u{f07b} /work/bmux · \u{e0a0} main*");
        assert_eq!(detail(23), "\u{f07b} bmux · \u{e0a0} main*");
        assert_eq!(detail(20), "bmux · \u{e0a0} main*");
    }

    /// Workspaces take their configured colors, others one from their name.
    #[test]
    fn workspace_colors_can_be_configured() {
        let parsed = Settings::parse(Some(&toml::Value::Table(toml::toml! {
            [workspace_colors]
            bmux = "bright-green"
            notes = "#ff8800"
        })))
        .unwrap();
        assert_eq!(
            parsed.workspace_colors.get("bmux"),
            Some(&RenderColor::Named(RenderNamedColor::BrightGreen))
        );
        let state = grouped_state(
            Settings {
                workspace_colors: parsed.workspace_colors,
                ..grouped()
            },
            None,
        );
        let surface = build_surface(&state, 1);
        let swatch = painted_row(&surface, 1)
            .into_iter()
            .find(|(x, ..)| *x == 0)
            .and_then(|(.., style)| style.fg);
        assert_eq!(
            swatch,
            Some(RenderColor::Named(RenderNamedColor::BrightGreen))
        );
        assert!(
            Settings::parse(Some(&toml::Value::Table(toml::toml! {
                [workspace_colors]
                bmux = "teal"
            })))
            .is_err()
        );
    }

    #[test]
    fn a_fresh_idle_agent_shows_its_name_without_a_stopped_count() {
        let mut state = grouped_state(grouped(), None);
        let mut snapshot = state.activity.snapshot().unwrap().clone();
        let docs = &mut snapshot.tabs[2];
        docs.state = tabs_status::TabState::Idle;
        docs.state_since_ms = None;
        docs.agent.as_mut().unwrap().finished_at_ms = None;
        docs.running_command = None;
        state.activity.replace(snapshot);
        let text = surface_text(&build_surface(&state, 1), 32, 6);
        let row = text.iter().find(|line| line.contains("docs")).unwrap();
        assert!(
            row.contains("○ docs") && row.ends_with("claude"),
            "{text:?}"
        );
        assert!(!text[0].contains('■') && !text[1].contains('■'), "{text:?}");
    }

    /// A finished unseen tab shows a check mark, a filled done badge, and
    /// its age. Both the workspace header and the sidebar heading count it.
    #[test]
    fn a_finished_unseen_tab_shows_done() {
        use bmux_tabs_plugin_api::tabs_status::{AgentActivity, TabState as Published};
        let mut state = grouped_state(grouped(), None);
        let mut snapshot = state.activity.snapshot().unwrap().clone();
        let docs = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.tab_id == Uuid::from_u128(3))
            .unwrap();
        docs.state = Published::Done;
        docs.state_since_ms = Some(snapshot.now_ms - 120_000);
        assert_eq!(
            docs.agent.as_ref().map(|agent| agent.activity),
            Some(AgentActivity::Idle)
        );
        state.activity.replace(snapshot);
        let surface = build_surface(&state, 1);
        let text = surface_text(&surface, 32, 6);
        let row = text.iter().position(|line| line.contains("docs")).unwrap();
        assert!(text[row].contains("✓ docs"), "{text:?}");
        assert!(text[row].ends_with("done  2m"), "{text:?}");
        let runs = painted_row(&surface, u16::try_from(row).unwrap());
        let glyph = runs
            .iter()
            .find(|(_, text, _)| text.starts_with('✓'))
            .and_then(|(.., style)| style.fg);
        let color = Some(activity::TabState::Done.color());
        assert_eq!(glyph, color);
        let (_, _, badge) = runs
            .iter()
            .find(|(_, text, _)| text == " done ")
            .expect("done badge");
        assert_eq!(badge.bg, color);
        assert!(badge.bold);
        assert!(text[0].contains("✓1"), "the heading counts it: {text:?}");
        assert!(text[1].contains("✓1"), "its group's header too: {text:?}");
    }

    /// Scrolled into a group, its header stays pinned at the top of the list,
    /// and only it can be clicked there.
    #[test]
    fn a_groups_header_stays_pinned_while_its_tabs_scroll() {
        let mut state = grouped_state(grouped(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 5));
        let header_row = |state: &CompanionState| {
            let surface = build_surface(state, 1);
            let text = surface_text(&surface, 32, 5);
            let regions = surface
                .interactive_regions
                .iter()
                .filter(|region| region.rect.y == 1)
                .map(|region| region.local_id.clone())
                .collect::<Vec<_>>();
            (text[1].clone(), regions)
        };
        assert!(state.scroll_rows(2));
        let (text, regions) = header_row(&state);
        assert!(text.contains("bmux"), "{text:?}");
        assert_eq!(regions, [format!("group:{}", Uuid::from_u128(100))]);
        // Scrolled as far as it goes, the last tabs of the group are on
        // screen and its header still is.
        assert!(state.scroll_rows(10));
        let (text, regions) = header_row(&state);
        assert!(text.contains("bmux"), "{text:?}");
        assert_eq!(regions, [format!("group:{}", Uuid::from_u128(100))]);
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
                " TABS              ◆1 ◉1 ■1 ▾ ×",
                "▌ bmux               ◆1 ◉1 ■1 ▸",
                "",
                "▌ notes                     1 ▾",
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
                .group_collapsed(notes)
                .unwrap()
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
            // The heading sums up only the listed tabs.
            assert_eq!(
                surface_text(&build_surface(&state, 1), 32, 1)[0],
                " TABS              ◆1 ◉1 ■1 ▾ ×"
            );
            state.select(Some(Uuid::from_u128(4)));
            assert_eq!(names(&state), ["todo"], "{grouping:?}");
        }
    }

    #[test]
    fn follow_focus_keeps_pins_open_and_reapplies_on_workspace_change() {
        let options = toml::Value::Table(toml::toml! {
            grouping = "workspace"
            workspace_visibility = "follow_focus"
            pinned_workspaces = ["notes"]
        });
        let mut state = grouped_state(Settings::parse(Some(&options)).unwrap(), None);
        let text =
            |state: &CompanionState| surface_text(&build_surface(state, 1), 32, 12).join("\n");
        assert!(text(&state).contains("todo"));
        state.toggle_group(Uuid::from_u128(200));
        state.select(Some(Uuid::from_u128(2)));
        assert!(!text(&state).contains("todo"));
        state.select(Some(Uuid::from_u128(4)));
        assert!(!text(&state).contains("build"));
        assert!(text(&state).contains("◆1 ◉1 ■1 ▸"));
        state.toggle_group(Uuid::from_u128(100));
        assert!(text(&state).contains("build"));
        state.select(Some(Uuid::from_u128(2)));
        state.select(Some(Uuid::from_u128(4)));
        assert!(!text(&state).contains("build"));
    }

    #[test]
    fn manual_expansion_bypasses_auto_collapse_after_restart_until_focus_moves() {
        let dir = scratch_dir();
        let settings = Settings {
            workspace_visibility: WorkspaceVisibility::FollowFocus,
            ..grouped()
        };
        let notes = Uuid::from_u128(200);
        let shown = |state: &CompanionState| {
            surface_text(&build_surface(state, 1), 32, 12)
                .iter()
                .any(|line| line.contains("todo"))
        };
        let mut state = grouped_state(settings.clone(), Some(dir.clone()));
        assert!(!shown(&state));
        assert!(state.toggle_group(notes).is_none());
        let mut restored = grouped_state(settings.clone(), Some(dir.clone()));
        assert!(shown(&restored));
        restored.select(Some(Uuid::from_u128(4)));
        restored.select(Some(Uuid::from_u128(1)));
        assert!(!shown(&grouped_state(settings, Some(dir.clone()))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn follow_attention_opens_background_groups_and_filter_keeps_their_full_counts() {
        let options = toml::Value::Table(toml::toml! {
            grouping = "workspace"
            workspace_visibility = "follow_focus_and_attention"
            other_workspace_tabs = "active"
        });
        let settings = Settings::parse(Some(&options)).unwrap();
        let mut state = grouped_state(settings, None);
        let text =
            |state: &CompanionState| surface_text(&build_surface(state, 1), 32, 12).join("\n");
        state.select(Some(Uuid::from_u128(4)));
        let rendered = text(&state);
        assert!(rendered.contains("◆1 ◉1 ■1 ▾"));
        assert!(
            rendered.contains("review") && rendered.contains("build") && rendered.contains("todo")
        );
        assert!(!rendered.contains("docs"));
        let update = |state: &mut CompanionState, tab_state| {
            let mut status = state.activity.snapshot().unwrap().clone();
            status.tabs[1].state = tab_state;
            status.revision += 1;
            state.apply_changes(Changes {
                tabs: None,
                personal: None,
                selection: None,
                status: Some(status),
                appearance: None,
                ticked: false,
                animated: false,
            });
        };
        update(&mut state, tabs_status::TabState::Idle);
        assert!(text(&state).contains("◉1 ■1 ▸"));
        update(&mut state, tabs_status::TabState::Failed);
        assert!(text(&state).contains("▲1 ◉1 ■1 ▾"));
        // Finished unseen deserves a look too: open, and listed.
        update(&mut state, tabs_status::TabState::Done);
        let rendered = text(&state);
        assert!(rendered.contains("✓1 ◉1 ■1 ▾"), "{rendered}");
        assert!(rendered.contains("review"), "{rendered}");
        update(&mut state, tabs_status::TabState::Idle);
        let rendered = text(&state);
        assert!(rendered.contains("◉1 ■1 ▸"));
        assert!(!rendered.contains("build"));
    }

    /// A narrow header keeps the most pressing counts.
    #[test]
    fn a_narrow_header_drops_the_least_pressing_counts_first() {
        let mut state = grouped_state(grouped(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 16, 14));
        let header = surface_text(&build_surface(&state, 1), 16, 2)[1].clone();
        assert!(header.ends_with("◆1 ◉1 ▾"), "{header:?}");
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
                    state: bmux_tabs_plugin_api::tabs_status::TabState::Working,
                    state_since_ms: Some(now - 1_000),
                    panes: 1,
                    shell_command: None,
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
                    content.ends_with("working now"),
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
        assert_eq!(width(&owner), 36);
        assert_eq!(offset(&owner), scrolled);
        // Sideways from the thumb resizes too: the first movement decides.
        let thumb = {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            state.resize(ExtensionRect::new(0, 0, 36, 8));
            let surface = build_surface(state, 2);
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
                    bmux_plugin::layout::LayoutOperation::Split {
                        extent:
                            LayoutExtent::Cells(width)
                            | LayoutExtent::Reserving {
                                preferred: width, ..
                            },
                        ..
                    } => Some(width),
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
        assert_eq!(sidebar.requested_width(), 2);
        sidebar.reallocate(ExtensionRect::new(0, 0, 2, 20));
        assert_eq!(sidebar.edge().rect.x, 1);
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
        sidebar.handled("drag", 99);
        assert_eq!(sidebar.requested_width(), 2);
        sidebar.reallocate(ExtensionRect::new(98, 0, 2, 20));
        assert_eq!(sidebar.edge().rect.x, 0);
        sidebar.handled("drag", 80);
        assert_eq!(sidebar.requested_width(), 20);
        sidebar.handled("up", 80);
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sidebar_controls_minimize_restore_and_close_the_attachment() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        let allocate = |width| {
            let resolved = bmux_plugin::layout::resolve_plugin_layout(
                ExtensionRect::new(0, 0, width, 20),
                (1, 1),
                &sidebar.layouts.requests(),
            )
            .unwrap();
            sidebar.reallocate(resolved.allocations[0].rect);
            resolved.remaining.w
        };
        let surface = || {
            sidebar
                .surfaces
                .owner_snapshot(OWNER)
                .unwrap()
                .surfaces
                .remove(0)
        };
        let click = |control| {
            let id = item_region_id(SidebarHit::Control(control));
            let snapshot = sidebar.surfaces.owner_snapshot(OWNER).unwrap();
            let region = snapshot.surfaces[0]
                .interactive_regions
                .iter()
                .find(|region| region.local_id == id)
                .unwrap();
            let mut event = press(&id);
            let target = event.target.as_mut().unwrap();
            target.revision = snapshot.revision;
            target.key = region.key;
            let result = sidebar.input.dispatch(&input_endpoint(), &event);
            assert!(
                matches!(result, bmux_plugin::AttachPresentationInputDispatch::Handled(result) if result.consumed && result.dirty)
            );
        };
        assert!(surface_text(&surface(), 28, 1)[0].ends_with("▾ ×"));
        click(WindowControl::Minimize);
        assert_eq!(sidebar.requested_width(), 2);
        assert_eq!(allocate(100), 98);
        assert_eq!(surface_text(&surface(), 2, 1)[0], "▸│");
        assert!(
            surface()
                .interactive_regions
                .iter()
                .all(|region| region.local_id
                    != item_region_id(SidebarHit::Control(WindowControl::Close)))
        );
        click(WindowControl::Minimize);
        assert_eq!(sidebar.requested_width(), 28);
        assert_eq!(allocate(100), 72);

        // The host can allocate the strip in responsive mode too.
        assert_eq!(allocate(60), 58);
        assert_eq!(surface_text(&surface(), 2, 1)[0], "▸│");
        assert_eq!(allocate(100), 72);
        assert!(surface_text(&surface(), 28, 1)[0].ends_with("▾ ×"));

        click(WindowControl::Close);
        assert_eq!(allocate(60), 60);
        assert!(matches!(
            sidebar.layouts.requests()[0].operation,
            bmux_plugin::layout::LayoutOperation::Hidden
        ));
        assert_eq!(sidebar.surfaces.owner_snapshot(OWNER).unwrap().surfaces, []);
        assert!(
            sidebar
                .input
                .command(OWNER, "toggle", &[])
                .unwrap()
                .consumed
        );
        assert_eq!(sidebar.requested_width(), 28);
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Clicks `control` on the sidebar's latest published surface.
    fn click_control(sidebar: &Installed, control: WindowControl) {
        let id = item_region_id(SidebarHit::Control(control));
        let snapshot = sidebar.surfaces.owner_snapshot(OWNER).unwrap();
        let region = snapshot.surfaces[0]
            .interactive_regions
            .iter()
            .find(|region| region.local_id == id)
            .unwrap();
        let mut event = press(&id);
        let target = event.target.as_mut().unwrap();
        target.revision = snapshot.revision;
        target.key = region.key;
        let result = sidebar.input.dispatch(&input_endpoint(), &event);
        assert!(
            matches!(result, bmux_plugin::AttachPresentationInputDispatch::Handled(result) if result.consumed && result.dirty)
        );
    }

    /// Resolve the sidebar's layout in a terminal `width` columns wide, as the
    /// host does each frame, and hand it its allocation. Returns the sidebar's
    /// width and the columns left for the panes.
    fn host_frame(sidebar: &Installed, width: u16) -> (u16, u16) {
        let resolved = bmux_plugin::layout::resolve_plugin_layout(
            ExtensionRect::new(0, 0, width, 20),
            (1, 1),
            &sidebar.layouts.requests(),
        )
        .unwrap();
        let rect = resolved.allocations[0].rect;
        sidebar.reallocate(rect);
        (rect.w, resolved.remaining.w)
    }

    /// On a terminal narrower than `collapse_below_width`, one restore of a
    /// minimized sidebar shows its tabs, not the host's responsive strip.
    #[test]
    fn one_restore_expands_a_minimized_sidebar_on_a_narrow_terminal() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        let heading = |sidebar: &Installed, width| {
            let surface = sidebar
                .surfaces
                .owner_snapshot(OWNER)
                .unwrap()
                .surfaces
                .remove(0);
            surface_text(&surface, width, 1)[0].clone()
        };
        // Narrow: the host shows the strip; restoring it expands it.
        assert_eq!(host_frame(&sidebar, 60), (2, 58));
        click_control(&sidebar, WindowControl::Minimize);
        assert_eq!(host_frame(&sidebar, 60), (28, 32));
        click_control(&sidebar, WindowControl::Minimize);
        assert_eq!(host_frame(&sidebar, 60), (2, 58));
        // One restore: the host's next frames show the tabs.
        click_control(&sidebar, WindowControl::Minimize);
        host_frame(&sidebar, 60);
        assert_eq!(host_frame(&sidebar, 60), (28, 32));
        assert!(heading(&sidebar, 28).starts_with(" TABS"));
        // On a wide terminal, a restore keeps the responsive width.
        click_control(&sidebar, WindowControl::Minimize);
        assert_eq!(host_frame(&sidebar, 100), (2, 98));
        click_control(&sidebar, WindowControl::Minimize);
        assert_eq!(host_frame(&sidebar, 100), (28, 72));
        assert!(matches!(
            sidebar.layouts.requests()[0].operation,
            bmux_plugin::layout::LayoutOperation::Split {
                extent: LayoutExtent::Responsive { .. },
                ..
            }
        ));
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An expanded sidebar never takes the columns the panes need: on a
    /// narrow terminal it gives way to leave them [`PANE_RESERVE`].
    #[test]
    fn an_expanded_sidebar_leaves_the_panes_room() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        assert_eq!(host_frame(&sidebar, 60), (2, 58));
        click_control(&sidebar, WindowControl::Minimize);
        // Widened past what the terminal can spare.
        for _ in 0..10 {
            sidebar.input.command(OWNER, "widen", &[]).unwrap();
        }
        assert_eq!(sidebar.requested_width(), 48);
        assert_eq!(host_frame(&sidebar, 60), (40, 20));
        assert_eq!(host_frame(&sidebar, 44), (24, 20));
        // Too narrow for both: the strip, not nothing.
        assert_eq!(host_frame(&sidebar, 10), (2, 8));
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Widening a minimized sidebar starts from the width it remembers.
    #[test]
    fn widening_a_minimized_sidebar_starts_from_its_remembered_width() {
        let dir = scratch_dir();
        let sidebar = Installed::new("left", ExtensionRect::new(0, 0, 28, 20), &dir);
        assert_eq!(host_frame(&sidebar, 100), (28, 72));
        click_control(&sidebar, WindowControl::Minimize);
        assert_eq!(sidebar.requested_width(), 2);
        sidebar.input.command(OWNER, "widen", &[]).unwrap();
        assert_eq!(sidebar.requested_width(), 30);
        assert_eq!(host_frame(&sidebar, 100), (30, 70));
        drop(sidebar);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Switching tabs brings the shown one into view whatever the workspace
    /// settings, including those that keep the scroll on other changes.
    #[test]
    fn switching_tabs_reveals_the_shown_tab_with_every_workspace_setting() {
        for options in [
            "",
            "grouping = 'workspace'\nworkspace_visibility = 'follow_focus'",
            "grouping = 'workspace'\nother_workspace_tabs = 'active'",
        ] {
            let mut settings =
                Settings::parse(Some(&toml::from_str::<toml::Value>(options).unwrap())).unwrap();
            settings.visibility.visible = true;
            settings.working_indicator = WorkingIndicator::Static;
            let mut state = CompanionState::new(settings, None);
            state.allocation = Some(ExtensionRect::new(0, 0, 32, 8));
            let tabs = |shown: u128| tabs_list::TabListSnapshot {
                tabs: (0..12)
                    .map(|index| tabs_list::TabListEntry {
                        id: Uuid::from_u128(index),
                        name: format!("tab-{index}"),
                        active: index == shown,
                        workspace: "default".to_string(),
                        workspace_id: Uuid::nil(),
                    })
                    .collect(),
                revision: 1,
            };
            state.replace_tabs(tabs(0));
            assert_eq!(state.scroll_offset(), 0, "{options:?}");
            state.replace_tabs(tabs(11));
            assert!(state.scroll_offset() > 0, "{options:?}");
            let text = surface_text(&build_surface(&state, 1), 32, 8).join("\n");
            assert!(text.contains("tab-11"), "{options:?}: {text}");
        }
    }

    /// The spinner animates only while a working tab's card is listed, not
    /// for one in another workspace's scope or under a collapsed header.
    #[test]
    fn only_a_listed_working_card_animates() {
        let spinning = |settings: Settings| Settings {
            working_indicator: WorkingIndicator::Spinner,
            ..settings
        };
        let state = grouped_state(spinning(grouped()), None);
        assert!(state.animates());
        let mut scoped = grouped_state(
            spinning(Settings {
                scope: Scope::CurrentWorkspace,
                ..grouped()
            }),
            None,
        );
        scoped.select(Some(Uuid::from_u128(4)));
        assert!(!scoped.animates());
        let mut collapsed = grouped_state(spinning(grouped()), None);
        collapsed.toggle_group(Uuid::from_u128(100));
        assert!(!collapsed.animates());
        collapsed.toggle_group(Uuid::from_u128(100));
        assert!(collapsed.animates());
    }

    /// A detail that fits the list exactly is shown whole, also while the
    /// scrollbar takes a column; one that does not shortens its directory
    /// instead of being cut.
    #[test]
    fn the_detail_line_fits_the_list_beside_the_scrollbar() {
        let detail = |cwd: &str| {
            let mut state = fixture_with_details(Settings {
                icons: false,
                ..shown_settings()
            });
            let mut status = state.activity.snapshot().unwrap().clone();
            status.tabs[0].cwd = Some(cwd.into());
            state.activity.replace(status);
            // Two two-row cards in two rows: the scrollbar shows.
            state.allocation = Some(ExtensionRect::new(0, 0, 32, 4));
            state.settle_scroll(false);
            let surface = build_surface(&state, 1);
            assert!(state.list_columns(2).1.is_some(), "the scrollbar shows");
            // Left of the scrollbar's column.
            surface_text(&surface, 31, 3)[2].trim().to_string()
        };
        // 25 cells: the list's 31 columns less the detail's indent.
        assert_eq!(detail("/work/abcdefghijk"), "/work/abcdefghijk · main*");
        assert_eq!(detail("/work/abcdefghijkl"), "abcdefghijkl · main*");
    }

    #[test]
    fn strip_summarizes_live_status_across_workspaces_even_with_current_workspace_scope() {
        let mut state = grouped_state(
            Settings {
                scope: Scope::CurrentWorkspace,
                ..grouped()
            },
            None,
        );
        state.select(Some(Uuid::from_u128(4)));
        let mut statuses = state.activity.snapshot().unwrap().tabs.clone();
        statuses
            .iter_mut()
            .find(|tab| tab.tab_id == Uuid::from_u128(1))
            .unwrap()
            .shell_command = Some("cargo build".into());
        statuses
            .iter_mut()
            .find(|tab| tab.tab_id == Uuid::from_u128(4))
            .unwrap()
            .shell_command = Some("sleep 30".into());
        state.activity.replace(tabs_status::TabStatusSnapshot {
            tabs: statuses,
            now_ms: 600_000,
            revision: 2,
        });
        let card = state.activity.activity(Uuid::from_u128(4)).unwrap();
        assert_eq!(card.state, activity::TabState::Shell);
        assert_eq!(card.summary.as_deref(), Some("sleep"));
        let labels = |indices: &[usize]| {
            group_summary(&state, indices.iter().copied(), 30)
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&[0, 1, 2]), ["◆1", "◉1", "›1", "■1"]);
        assert_eq!(labels(&[0, 1, 2, 3]), ["◆1", "◉1", "›2", "■1"]);
        state.resize(ExtensionRect::new(0, 0, 2, 5));
        let text = |state: &CompanionState| surface_text(&build_surface(state, 1), 2, 5);
        assert_eq!(text(&state), ["▸│", "◆│", "◉│", "›│", "■│"]);
        state.apply_changes(Changes {
            tabs: None,
            personal: None,
            selection: None,
            status: Some(tabs_status::TabStatusSnapshot {
                tabs: vec![tabs_status::TabStatus {
                    tab_id: Uuid::from_u128(4),
                    state: tabs_status::TabState::Failed,
                    ..idle_status()
                }],
                now_ms: 600_001,
                revision: 3,
            }),
            appearance: None,
            ticked: false,
            animated: false,
        });
        assert_eq!(text(&state), ["▸│", "▲│", " │", " │", " │"]);
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
        assert_eq!(sidebar.requested_width(), 2);
        drop(sidebar);
        assert_eq!(
            std::fs::read_to_string(dir.join(WIDTH_FILE))
                .unwrap()
                .trim(),
            "4"
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
                        state: bmux_tabs_plugin_api::tabs_status::TabState::Working,
                        state_since_ms: None,
                        panes: 2,
                        shell_command: None,
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
            AgentActivity, GitStatus, TabAgent, TabState, TabStatus, TabStatusSnapshot,
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
            ..idle_status()
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
                    state: TabState::Working,
                    state_since_ms: Some(now - 5_000),
                    ..status(2)
                },
                TabStatus {
                    last_exit: Some(101),
                    cwd: Some("/work/build".into()),
                    state: TabState::Failed,
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
                    state: TabState::NeedsInput,
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
                " TABS              ◆1 ▲1 ◉1 ▾ ×",
                "▎ ○ editor                 nvim",
                "▎   \u{f07b} /work/bmux · \u{e0a0} main* ↑2",
                "  ◉ claude           working 5s",
                "    \u{f07b} /work/bmux/plugins",
                "  ▲ build             exit 101",
                "    \u{f07b} /work/build",
                "  ◆ review         needs input",
                "    approve edit?",
                "",
            ],
            "{}",
            text.join("\n")
        );
    }

    /// A card's painted state glyph with its color, and its summary.
    type PaintedCard = (Option<(String, Option<RenderColor>)>, String);

    /// Each named card's painted state glyph with its color, and what its
    /// first line says after the name (its summary).
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
                    .split_once(*name)
                    .map_or("", |(_, rest)| rest)
                    .trim()
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
            AgentActivity, TabAgent, TabState, TabStatus, TabStatusSnapshot,
        };
        let now = 600_000;
        let agents = [
            ("working", TabState::Working, AgentActivity::Working, None),
            ("asking", TabState::NeedsInput, AgentActivity::Blocked, None),
            (
                "stopped",
                TabState::Stopped,
                AgentActivity::Idle,
                Some(now - 60_000),
            ),
        ];
        let mut state = CompanionState::new(shown_settings(), None);
        state.allocation = Some(ExtensionRect::new(0, 0, 32, 10));
        state.activity.replace(TabStatusSnapshot {
            tabs: (1..=3_u128)
                .zip(&agents)
                .map(|(index, (_, state, activity, finished_at_ms))| TabStatus {
                    tab_id: Uuid::from_u128(index),
                    state: *state,
                    state_since_ms: Some(now - 60_000),
                    panes: 1,
                    shell_command: None,
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
        };
        select(&mut state, 1);
        let first = painted(&state);
        let summaries = first
            .iter()
            .map(|(_, summary)| summary.as_str())
            .collect::<Vec<_>>();
        assert_eq!(summaries, ["working 1m", "needs input", "stopped 1m"]);
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
        assert!(state.resize(ExtensionRect::new(0, 0, 20, 5)));
        assert_eq!(state.list_height(&state.item_keys()), 3);
        assert_eq!(state.allocated_width(), 20);
        // Taller, the offset shrinks so the last card still ends the list.
        assert_eq!(state.scroll_offset(), 3);
        assert!(!state.resize(ExtensionRect::new(0, 0, 20, 5)));
        // Narrow, cards are one row and all of them fit.
        assert!(state.resize(ExtensionRect::new(0, 0, 12, 5)));
        assert_eq!(state.scroll_offset(), 0);
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
        let tabs = tab_regions(&surface);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].local_id, format!("tab:{id}"));
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
