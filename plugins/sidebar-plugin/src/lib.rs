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

use bmux_plugin::layout::{
    LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest, PluginLayoutSnapshot,
};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceSnapshot,
};
use bmux_plugin::{
    AttachInputEvent, AttachInputResult, BorderGlyphs, ExtensionRect, RenderNamedColor, RenderOp,
    RenderStyle, ServiceCallerDispatchClient, block_on_typed_dispatch,
};
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use bmux_presentation_scene::scene::{HitTable, HitTables};
use bmux_presentation_state::visibility::{
    PresentationVisibility, VisibilityCommand, VisibilitySettings,
};
use bmux_presentation_state::{PresentationEntityRef, PresentationFact, PresentationFactRole};
use bmux_tabs_plugin_api::{tabs_commands, tabs_list};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    placement: Placement,
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
            title_template: "{marker} {index}{name}".to_string(),
            description_template: String::new(),
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

#[derive(Debug, Clone)]
struct MeasuredSidebarItem {
    title: String,
    description: String,
    status: String,
    width: usize,
    fields: Vec<bmux_tui::composition::TextBlock>,
    layout: bmux_tui::component::LayoutNode,
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
    snapshot: tabs_list::TabListSnapshot,
    hovered_tab_id: Option<Uuid>,
    scroll: bmux_tui_components::scroll_view::ScrollViewState,
    measured_items: std::cell::RefCell<std::collections::BTreeMap<Uuid, MeasuredSidebarItem>>,
    /// What each published revision's region keys mean.
    hits: std::cell::RefCell<HitTables<SidebarHit>>,
}

/// The typed meaning of a sidebar region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarHit {
    Tab(Uuid),
}

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
        Self {
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
            snapshot: tabs_list::TabListSnapshot {
                tabs: Vec::new(),
                revision: 0,
            },
            hovered_tab_id: None,
            scroll: bmux_tui_components::scroll_view::ScrollViewState::new(),
            measured_items: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        }
    }

    const fn visible(&self) -> bool {
        self.visibility.visible()
    }

    const fn allocated_width(&self) -> u16 {
        match self.allocation {
            Some(rect) => rect.w,
            None => self.settings.width,
        }
    }

    fn resize(&mut self, rect: ExtensionRect) -> bool {
        if self.allocation == Some(rect) {
            return false;
        }
        self.allocation = Some(rect);
        let layout = self.scroll_layout();
        bmux_tui_components::scroll_view::ScrollView::scroll_vertical_by(
            &layout,
            &mut self.scroll,
            0,
        );
        if let Some(index) = self.snapshot.tabs.iter().position(|tab| tab.active) {
            self.reveal(index);
        }
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

    fn measure_item(
        &self,
        id: Uuid,
        title: String,
        description: String,
        status: String,
        width: usize,
    ) -> std::cell::Ref<'_, MeasuredSidebarItem> {
        let stale = self.measured_items.borrow().get(&id).is_none_or(|item| {
            item.title != title
                || item.description != description
                || item.status != status
                || item.width != width
        });
        if stale {
            use bmux_tui::component::{Component, Constraints, LayoutCx};
            use bmux_tui::composition::{Column, TextBlock};
            use bmux_tui::text::{Line, Text};
            let description_lines = if description.trim().is_empty() || width <= 1 {
                Vec::new()
            } else {
                bmux_tui::text::wrap_text(
                    description.trim(),
                    bmux_tui::text::TextWrapGeometry::uniform(width - 1),
                    bmux_tui::text::TextWrap::Word,
                )
                .into_iter()
                .take(2)
                .collect::<Vec<_>>()
            };
            let mut fields = vec![
                TextBlock::new(Text::from_lines(vec![Line::raw(truncate_to_width(
                    &title, width,
                ))]))
                .id("title"),
            ];
            if !description_lines.is_empty() {
                fields.push(
                    TextBlock::new(Text::from_lines(
                        description_lines
                            .iter()
                            .map(|line| Line::raw(format!(" {line}")))
                            .collect::<Vec<_>>(),
                    ))
                    .id("description"),
                );
            }
            if !status.is_empty() {
                fields.push(
                    TextBlock::new(Text::from_lines(vec![Line::raw(format!(
                        " {}",
                        truncate_to_width(&status, width.saturating_sub(1))
                    ))]))
                    .id("status"),
                );
            }
            let content = fields
                .iter()
                .fold(Column::new().id(format!("tab:{id}")), |column, field| {
                    column.child(field.clone())
                });
            let layout = content.layout(
                Constraints::for_width(u64::from(u16::try_from(width).unwrap_or(u16::MAX))),
                &mut LayoutCx::new(),
            );
            self.measured_items.borrow_mut().insert(
                id,
                MeasuredSidebarItem {
                    title,
                    description,
                    status,
                    width,
                    fields,
                    layout,
                },
            );
        }
        std::cell::Ref::map(self.measured_items.borrow(), |items| &items[&id])
    }

    fn tab_fact(&self, tab: &tabs_list::TabListEntry) -> Option<PresentationFact> {
        self.facts
            .as_ref()
            .and_then(|facts| facts.with(|registry| tab_fact(tab, registry)))
            .flatten()
    }

    fn measured_tab(&self, index: usize) -> std::cell::Ref<'_, MeasuredSidebarItem> {
        let tab = &self.snapshot.tabs[index];
        let fact = self.tab_fact(tab);
        let render = |template: &str| {
            render_template(
                template,
                tab,
                index,
                self.settings.show_index,
                fact.as_ref(),
            )
        };
        self.measure_item(
            tab.id,
            render(&self.settings.title_template),
            render(&self.settings.description_template),
            render(&self.settings.status_template),
            usize::from(self.allocated_width().saturating_sub(4)),
        )
    }

    fn scroll_layout(&self) -> bmux_tui::component::LayoutNode {
        use bmux_tui::component::{ChildLayout, LayoutId, LayoutNode, LogicalSize};
        let mut content = LayoutNode::leaf(
            LayoutId::new("sidebar.items"),
            LogicalSize::new(u64::from(self.settings.width), 0),
        );
        for index in 0..self.snapshot.tabs.len() {
            let item = self.measured_tab(index);
            let y = content.size.height;
            content.size.height = y.saturating_add(item.layout.size.height);
            content
                .children
                .push(ChildLayout::new(0, y, item.layout.clone()));
        }
        let viewport_height = content
            .children
            .iter()
            .take(self.settings.maximum_visible_items)
            .map(|child| child.node.size.height)
            .sum();
        bmux_tui_components::scroll_view::ScrollViewComponent::viewport_layout(
            LayoutId::new("sidebar.viewport"),
            LogicalSize::new(
                u64::from(self.allocated_width()),
                self.allocation.map_or(viewport_height, |rect| {
                    u64::try_from(usize::from(rect.h.saturating_sub(2))).unwrap_or(u64::MAX)
                }),
            ),
            content,
        )
    }

    fn reveal(&mut self, index: usize) {
        let layout = self.scroll_layout();
        let Some(item) = layout.children[0].node.children.get(index) else {
            return;
        };
        bmux_tui_components::scroll_view::ScrollView::new().ensure_visible(
            &layout,
            &mut self.scroll,
            item.y,
            item.node.size.height,
        );
    }

    fn replace_tabs(&mut self, snapshot: tabs_list::TabListSnapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            self.measured_items
                .get_mut()
                .retain(|id, _| self.snapshot.tabs.iter().any(|tab| tab.id == *id));
            let layout = self.scroll_layout();
            bmux_tui_components::scroll_view::ScrollView::scroll_vertical_by(
                &layout,
                &mut self.scroll,
                0,
            );
            if let Some(active) = self.snapshot.tabs.iter().position(|tab| tab.active) {
                self.reveal(active);
            }
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }
}

type CompanionHandle = std::sync::Arc<Mutex<Option<CompanionState>>>;

#[derive(Default)]
pub struct SidebarPlugin;

impl Plugin for SidebarPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(|_, a0, a1| Self::activate(a0, a1))
            .blocking_services(|_, a0| Self::invoke_service(a0), None);
        for name in VisibilityCommand::NAMES {
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

/// Reached only when no sidebar presentation answered a visibility command,
/// for example from a shell or in a client without the sidebar installed.
fn presentation_only_command() -> PluginCommandError {
    PluginCommandError::unavailable(
        "sidebar visibility commands only apply inside an attach client showing the sidebar",
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
        let mut facts_rx = facts.subscribe()?;
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
            companion.replace_tabs(initial.as_ref().clone());
            publish_companion(companion)?;
            let owner = std::sync::Arc::downgrade(&presentation.owner);
            companion.subscription = Some(handle.spawn(async move {
                loop {
                    let tabs = tokio::select! {
                        result = tabs_rx.changed() => {
                            if result.is_err() { break; }
                            Some(tabs_rx.borrow_and_update().as_ref().clone())
                        }
                        result = facts_rx.changed() => {
                            if result.is_err() { break; }
                            facts_rx.borrow_and_update();
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
                    if let Some(tabs) = tabs {
                        companion.replace_tabs(tabs);
                    } else {
                        // Fact text affects wrapping, row geometry and hit regions.
                        companion.measured_items.get_mut().clear();
                        companion.revision = companion.revision.saturating_add(1).max(1);
                        if let Some(active) =
                            companion.snapshot.tabs.iter().position(|tab| tab.active)
                        {
                            companion.reveal(active);
                        }
                    }
                    companion.presentation_pending.set(true);
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
            companion.hovered_tab_id = None;
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

fn layout_request(settings: &Settings, visible: bool) -> PluginLayoutRequest {
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
            preferred: settings.width,
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
    let request = layout_request(&companion.settings, companion.visible());
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

fn publish_surfaces(
    registry: &bmux_plugin::surface::PluginSurfaceRegistry,
    revision: u64,
    surfaces: Vec<PluginSurface>,
) -> Result<(), String> {
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

fn truncate_to_width(value: &str, maximum: usize) -> String {
    let mut result = String::new();
    let mut width = 0_usize;
    for grapheme in bmux_unicode_segmentation::graphemes(value) {
        let cell_width = bmux_cell_geometry::grapheme_width(grapheme);
        if width.saturating_add(cell_width) > maximum {
            break;
        }
        result.push_str(grapheme);
        width = width.saturating_add(cell_width);
    }
    result
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
        .replace('\u{0}', "{")
        .replace('\u{1}', "}")
}

#[cfg(test)]
fn push_wrapped_text(
    ops: &mut Vec<RenderOp>,
    text: &str,
    x: u16,
    start_row: u16,
    width: usize,
    maximum_rows: u16,
    style: RenderStyle,
) -> u16 {
    if text.is_empty() || width == 0 || maximum_rows == 0 {
        return 0;
    }
    let lines = bmux_tui::text::wrap_text(
        text.trim(),
        bmux_tui::text::TextWrapGeometry::uniform(width),
        bmux_tui::text::TextWrap::Word,
    );
    let mut rows = 0_u16;
    for line in lines.iter().take(usize::from(maximum_rows)) {
        ops.push(RenderOp::text_run(
            x,
            start_row.saturating_add(rows),
            line,
            style,
        ));
        rows = rows.saturating_add(1);
    }
    rows
}

#[allow(clippy::too_many_lines)] // Scene construction is one ordered retained projection; splitting obscures row accounting.
// The surface wire format uses text runs; let the component painter resolve
// clipping and grapheme cells before adapting its plain field to that format.
fn paint_sidebar_field(
    field: &bmux_tui::composition::TextBlock,
    child: &bmux_tui::component::ChildLayout,
    placement: &(u64, std::ops::Range<u64>),
    style: RenderStyle,
    ops: &mut Vec<RenderOp>,
    metadata: (
        &mut bmux_plugin::surface::PluginSurfaceMetadata,
        String,
        u64,
    ),
) {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::Component;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;

    let width = u16::try_from(child.node.size.width).unwrap_or(u16::MAX);
    if width == 0 {
        return;
    }
    let start = placement.0.saturating_add(child.y);
    let end = start.saturating_add(child.node.size.height);
    let visible_start = start.max(placement.1.start);
    let visible_end = end.min(placement.1.end);
    if visible_start >= visible_end {
        return;
    }
    let height = u16::try_from(visible_end - visible_start).unwrap_or(u16::MAX);
    let offset = i64::try_from(visible_start - start).unwrap_or(i64::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = Frame::new(&mut buffer);
    PaintCx::new(&mut frame).with_child(
        0,
        -offset,
        bmux_tui::paint::LocalRect::new(0, offset, width, height),
        |cx| {
            field.paint(&child.node, cx);
            field.register_selection(
                &child.node,
                cx,
                metadata.1.clone(),
                metadata.1.clone(),
                0,
                metadata.2,
            );
        },
    );
    let origin_x = 2_u16.saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX));
    let origin_y =
        u16::try_from(visible_start.saturating_sub(placement.1.start) + 1).unwrap_or(u16::MAX);
    let area = Rect::new(origin_x, origin_y, width, height);
    let selection = metadata
        .0
        .selection
        .as_mut()
        .expect("producer selection scene");
    selection.push_scope(
        bmux_tui::selection::SelectionScope::new(metadata.1.clone(), area).revision(metadata.2),
    );
    for fragment in frame.selection().fragments() {
        let mut fragment = fragment.clone();
        fragment.area.x = fragment.area.x.saturating_add(origin_x);
        fragment.area.y = fragment.area.y.saturating_add(origin_y);
        selection.push_fragment(fragment);
    }
    metadata.0.text.insert(
        metadata.1.clone().into(),
        field
            .text()
            .lines
            .iter()
            .map(bmux_tui::text::Line::plain_text)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    metadata
        .0
        .semantics
        .as_mut()
        .expect("producer semantic scene")
        .push(bmux_tui::semantic::SemanticRegion::new(
            metadata.1, area, "label",
        ));
    drop(frame);
    for (offset, cells) in buffer.cells().chunks(usize::from(width)).enumerate() {
        let text = cells
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        let logical_row = visible_start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let Some(projected) = bmux_tui_components::scroll_view::ScrollView::project_rows(
            &placement.1,
            logical_row..logical_row.saturating_add(1),
        ) else {
            continue;
        };
        ops.push(RenderOp::text_run(
            2_u16.saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
            u16::try_from(projected.start + 1).unwrap_or(u16::MAX),
            text.trim_end(),
            style,
        ));
    }
}

const fn sidebar_title_style(
    is_active: bool,
    hovered: bool,
    active: RenderStyle,
    inactive: RenderStyle,
) -> RenderStyle {
    if is_active {
        active
    } else if hovered {
        inactive
            .named_foreground(RenderNamedColor::BrightWhite)
            .named_background(RenderNamedColor::Blue)
    } else {
        inactive
    }
}

fn sidebar_region(
    id: Uuid,
    width: u16,
    row: u16,
    height: u16,
    hits: &mut HitTable<SidebarHit>,
) -> PluginSurfaceRegion {
    PluginSurfaceRegion::new(
        tab_region_id(id),
        ExtensionRect::new(1, row, width.saturating_sub(2), height.max(1)),
    )
    .endpoint(input_endpoint())
    .focusable(bmux_plugin::surface::PluginSurfaceCursor::Pointer)
    .key(hits.key(SidebarHit::Tab(id)))
}

#[cfg(test)]
fn build_surface(state: &CompanionState, revision: u64) -> PluginSurface {
    build_surface_with_hits(state, revision).0
}

// Paint and logical descriptions share the same measured scroll projection.
#[allow(clippy::too_many_lines)]
fn build_surface_with_hits(
    state: &CompanionState,
    revision: u64,
) -> (PluginSurface, HitTable<SidebarHit>) {
    let mut hits = HitTable::new();
    let width = state.allocated_width();
    let background = RenderStyle::new()
        .named_foreground(RenderNamedColor::White)
        .named_background(RenderNamedColor::Black);
    let active = RenderStyle::new()
        .named_foreground(RenderNamedColor::BrightWhite)
        .named_background(RenderNamedColor::Blue)
        .bold();
    let inactive = RenderStyle::new()
        .named_foreground(RenderNamedColor::White)
        .named_background(RenderNamedColor::Black);
    // Content-sized surfaces are finalized from the emitted item geometry below.
    let height = u16::MAX;
    let mut ops = vec![
        RenderOp::fill_rect(ExtensionRect::new(0, 0, width, height), ' ', background),
        RenderOp::border(
            ExtensionRect::new(0, 0, width, height),
            BorderGlyphs::square(),
            background,
        ),
        RenderOp::text_run(2, 0, format!(" {} ", state.settings.heading), active),
    ];
    let mut metadata = bmux_plugin::surface::PluginSurfaceMetadata {
        selection: Some(bmux_tui::selection::SelectionScene::new()),
        semantics: Some(bmux_tui::semantic::SemanticScene::new()),
        text: std::collections::BTreeMap::new(),
    };
    let mut regions = Vec::with_capacity(state.snapshot.tabs.len());
    let layout = state.scroll_layout();
    let visible = state.scroll.vertical_offset()
        ..state
            .scroll
            .vertical_offset()
            .saturating_add(layout.size.height);
    let mut row = 1_u16;
    for (index, tab) in state.snapshot.tabs.iter().enumerate() {
        let item = &layout.children[0].node.children[index];
        let item_end = item.y.saturating_add(item.node.size.height);
        let Some(projected) =
            bmux_tui_components::scroll_view::ScrollView::project_rows(&visible, item.y..item_end)
        else {
            continue;
        };
        let fact = state.tab_fact(tab);
        let start_row = u16::try_from(projected.start + 1).unwrap_or(u16::MAX);
        let measured = state.measured_tab(index);
        let item_style = fact.as_ref().map_or_else(
            || {
                sidebar_title_style(
                    tab.active,
                    state.hovered_tab_id == Some(tab.id),
                    active,
                    inactive,
                )
            },
            |fact| fact_style(fact.role, if tab.active { active } else { inactive }),
        );
        for (field, child) in measured.fields.iter().zip(&measured.layout.children) {
            let style = match child.node.id.as_str() {
                "title" => item_style,
                "status" if tab.active => active,
                _ => inactive.dim(),
            };
            paint_sidebar_field(
                field,
                child,
                &(item.y, visible.clone()),
                style,
                &mut ops,
                (
                    &mut metadata,
                    format!("{}:{}", tab.id, child.node.id.as_str()),
                    revision,
                ),
            );
        }
        row = u16::try_from(projected.end + 1).unwrap_or(u16::MAX);
        regions.push(sidebar_region(
            tab.id,
            width,
            start_row,
            row.saturating_sub(start_row),
            &mut hits,
        ));
    }
    if state.settings.content_height {
        let rect = ExtensionRect::new(0, 0, width, row.saturating_add(1));
        ops[0] = RenderOp::fill_rect(rect, ' ', background);
        ops[1] = RenderOp::border(rect, BorderGlyphs::square(), background);
    }
    let mut surface = PluginSurface::layout(
        PluginSurfaceId::new(OWNER, SURFACE_ID, RETAINED_ID),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        ops,
    )
    .opaque(true);
    surface.metadata = Some(metadata);
    for region in regions {
        surface = surface.interactive_region(region);
    }
    (surface, hits)
}

/// The tab an event is addressed to: through the hit table of the revision it
/// was published with when it carries a typed key (a retired revision resolves
/// to nothing), otherwise through its owner-local region id.
fn hit_tab(companion: &CompanionState, event: &AttachInputEvent) -> Option<Uuid> {
    let target = event.target.as_ref()?;
    match target.key {
        Some(key) => match companion.hits.borrow().resolve(target.revision, key)? {
            SidebarHit::Tab(id) => Some(*id),
        },
        None => region_tab_id(&target.region),
    }
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
        hit_tab(companion, event)
    };
    if companion.hovered_tab_id == hovered {
        return false;
    }
    let previous = companion.hovered_tab_id;
    companion.hovered_tab_id = hovered;
    companion.revision = companion.revision.saturating_add(1).max(1);
    if publish_companion(companion).is_err() {
        companion.hovered_tab_id = previous;
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
    let previous_scroll = companion.scroll;
    let previous = companion.scroll.vertical_offset();
    let layout = companion.scroll_layout();
    bmux_tui_components::scroll_view::ScrollView::scroll_vertical_by(
        &layout,
        &mut companion.scroll,
        if event.wheel_delta > 0 { -1 } else { 1 },
    );
    if previous == companion.scroll.vertical_offset() {
        return true;
    }
    companion.revision = companion.revision.saturating_add(1).max(1);
    if publish_companion(companion).is_err() {
        companion.scroll = previous_scroll;
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
    let target = hit_tab(companion, event)?;
    let index = companion
        .snapshot
        .tabs
        .iter()
        .position(|tab| tab.id == target)?;
    let previous_scroll = companion.scroll;
    let previous_hover = companion.hovered_tab_id;
    match event.key.as_deref()? {
        "up" => {
            let next = index.saturating_sub(1);
            companion.reveal(next);
            companion.hovered_tab_id = companion.snapshot.tabs.get(next).map(|tab| tab.id);
        }
        "down" => {
            let next = index
                .saturating_add(1)
                .min(companion.snapshot.tabs.len().saturating_sub(1));
            companion.reveal(next);
            companion.hovered_tab_id = companion.snapshot.tabs.get(next).map(|tab| tab.id);
        }
        _ => return None,
    }
    let dirty = {
        companion.revision = companion.revision.saturating_add(1).max(1);
        if publish_companion(companion).is_ok() {
            true
        } else {
            companion.scroll = previous_scroll;
            companion.hovered_tab_id = previous_hover;
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
    let activate_key = event.event_kind == "key"
        && matches!(event.phase.as_str(), "press" | "repeat")
        && event.key.as_deref() == Some("enter");
    let activate_pointer = event.event_kind == "pointer"
        && event.phase == "down"
        && event.button.as_deref() == Some("left");
    if !activate_key && !activate_pointer {
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
    match block_on_typed_dispatch(tabs_commands::client::switch_tab(
        &mut client,
        target.to_string(),
    )) {
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

    #[test]
    fn field_paints_visible_rows_beyond_terminal_coordinate_limit() {
        use bmux_tui::component::{ChildLayout, Component, Constraints, LayoutCx};
        let text = format!("{}deep", "row\n".repeat(70_000));
        let field = bmux_tui::composition::TextBlock::new(text.as_str());
        let layout = field.layout(Constraints::for_width(4), &mut LayoutCx::new());
        let child = ChildLayout::new(0, 0, layout);
        let mut ops = Vec::new();
        let mut metadata = bmux_plugin::surface::PluginSurfaceMetadata {
            selection: Some(bmux_tui::selection::SelectionScene::new()),
            semantics: Some(bmux_tui::semantic::SemanticScene::new()),
            text: std::collections::BTreeMap::new(),
        };
        super::paint_sidebar_field(
            &field,
            &child,
            &(0, 70_000..70_001),
            super::RenderStyle::new(),
            &mut ops,
            (&mut metadata, "deep-field".to_owned(), 1),
        );
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], super::RenderOp::TextRun { text, .. } if text == "deep"));
    }
    use super::*;

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
        let left = layout_request(&shown_settings(), true);
        let right = layout_request(
            &Settings {
                placement: Placement::Right,
                order: 50,
                ..shown_settings()
            },
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
            render_template(concat!("{", "fact}"), &tab, 0, false, fact.as_ref()),
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

        // Without remembering, the configured default applies.
        let forgetful = runtime_resources(one_tab(Uuid::from_u128(0x500)), Some(directory.clone()));
        let _forgetful =
            SidebarPresentation::install_for_runtime(Some(&shown_toml()), &forgetful).unwrap();
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
        assert_eq!(settings.title_template, "{marker} {index}{name}");
        assert_eq!(settings.collapse_below_width, 80);
        let invalid: toml::Value =
            toml::from_str("width = 10\nminimum_width = 12\nmaximum_width = 40").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn truncation_preserves_combining_graphemes() {
        assert_eq!(truncate_to_width("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn truncation_is_unicode_cell_safe() {
        assert_eq!(truncate_to_width("ab界cd", 4), "ab界");
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
            render_template("{{literal}} {index}{name} {active}", &tab, 1, true, None,),
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
                Some(&fact)
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
    fn descriptions_wrap_on_unicode_cell_boundaries() {
        let mut ops = Vec::new();
        let rows = push_wrapped_text(&mut ops, "alpha 界 beta", 0, 0, 7, 3, RenderStyle::new());
        assert_eq!(rows, 2);
        assert_eq!(ops.len(), 2);
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
        state.hovered_tab_id = Some(second);
        let after = build_surface(&state, 2);
        assert_eq!(before.ops[3], after.ops[3]);
        assert_ne!(before.ops[4], after.ops[4]);
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
        state.scroll.set_vertical_offset(1);
        state
            .publish_allocation(rect, |current| {
                assert_eq!(current.scroll.vertical_offset(), 1);
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
        };
        assert_eq!(
            hit_tab(&state, &event(Some(target.clone()))),
            Some(Uuid::from_u128(5))
        );
        let retired = bmux_plugin::AttachInputTarget {
            revision: 3,
            ..target.clone()
        };
        assert_eq!(hit_tab(&state, &event(Some(retired))), None);
        let unknown = bmux_plugin::AttachInputTarget {
            key: Some(bmux_presentation_scene::scene::HitKey(9)),
            ..target
        };
        assert_eq!(hit_tab(&state, &event(Some(unknown))), None);
        assert_eq!(hit_tab(&state, &event(None)), None);
        assert_eq!(
            region_tab_id(&tab_region_id(Uuid::from_u128(5))),
            Some(Uuid::from_u128(5))
        );
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
        };
        assert!(!update_hover(&owner, &event));
        assert_eq!(owner.lock().unwrap().as_ref().unwrap().hovered_tab_id, None);
        event.phase = "wheel".to_string();
        event.wheel_delta = -1;
        assert!(!update_scroll(&owner, &event));
        assert_eq!(
            owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .scroll
                .vertical_offset(),
            0
        );
        event.event_kind = "key".to_string();
        event.phase = "press".to_string();
        event.key = Some("down".to_string());
        assert!(!update_keyboard(&owner, &event).unwrap().dirty);
        {
            let mut guard = owner.lock().unwrap();
            let state = guard.as_mut().unwrap();
            assert_eq!(state.scroll.vertical_offset(), 0);
            assert_eq!(state.hovered_tab_id, None);
            state.surfaces = Some(std::sync::Arc::new(
                bmux_plugin::surface::PluginSurfaceRegistry::new(1),
            ));
            drop(guard);
        }
        // A retry must apply exactly one navigation, not accumulate failed moves.
        assert!(update_keyboard(&owner, &event).unwrap().dirty);
        let guard = owner.lock().unwrap();
        let state = guard.as_ref().unwrap();
        assert_eq!(state.hovered_tab_id, Some(Uuid::from_u128(2)));
        assert!(state.scroll.vertical_offset() > 0);
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
        let layout = state.scroll_layout();
        assert_eq!(layout.size.height, 2);
        assert_eq!(layout.children[0].node.size.height, 6);
        bmux_tui_components::scroll_view::ScrollView::scroll_vertical_by(
            &layout,
            &mut state.scroll,
            1,
        );
        let surface = build_surface(&state, 1);
        assert_eq!(surface.interactive_regions.len(), 2);
        assert_eq!(
            surface.interactive_regions[0].rect,
            ExtensionRect::new(1, 1, state.settings.width - 2, 1)
        );
        assert_eq!(
            surface.interactive_regions[1].rect,
            ExtensionRect::new(1, 2, state.settings.width - 2, 1)
        );
        assert!(
            surface
                .ops
                .iter()
                .filter_map(|op| match op {
                    RenderOp::TextRun { y, .. } => Some(*y),
                    _ => None,
                })
                .all(|y| y <= 2)
        );
        state.reveal(2);
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert_eq!(build_surface(&state, 2).interactive_regions.len(), 1);
        assert!(state.resize(ExtensionRect::new(0, 0, 12, 5)));
        assert_eq!(state.scroll_layout().size.height, 3);
        assert_eq!(state.allocated_width(), 12);
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert!(!state.resize(ExtensionRect::new(0, 0, 12, 5)));
        assert!(state.resize(ExtensionRect::new(0, 0, 12, 1)));
        assert_eq!(
            build_surface(&state, 3).interactive_regions,
            [] as [bmux_plugin::surface::PluginSurfaceRegion; 0]
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
        assert_eq!(state.scroll.vertical_offset(), 3);
        let surface = build_surface(&state, 1);
        assert_eq!(surface.interactive_regions.len(), 2);
        assert_eq!(
            surface.interactive_regions[1].local_id,
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
        assert_eq!(before.ops[3], after.ops[3]);
        assert_ne!(before.ops[4], after.ops[4]);
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
        assert_eq!(build_surface(&state, 1).interactive_regions.len(), 1);

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
        assert_eq!(surface.interactive_regions.len(), 32);
        assert_eq!(state.scroll.vertical_offset(), 1_968);
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
        assert!(matches!(
            surface.ops[0],
            RenderOp::FillRect {
                rect: ExtensionRect { h: 3, .. },
                ..
            }
        ));
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
