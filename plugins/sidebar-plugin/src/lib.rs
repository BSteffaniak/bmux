#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_plugin::layout::{
    LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest, PluginLayoutSnapshot,
    global_plugin_layout_registry,
};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceSnapshot,
    global_plugin_surface_registry,
};
use bmux_plugin::{
    AttachInputEvent, AttachInputResult, BorderGlyphs, ExtensionRect, RenderNamedColor, RenderOp,
    RenderStyle, ServiceCallerDispatchClient, block_on_typed_dispatch,
};
use bmux_plugin_sdk::prelude::*;
use bmux_presentation_state::{
    PresentationEntityRef, PresentationFact, PresentationFactRole,
    global_presentation_fact_host_service,
};
use bmux_windows_plugin_api::{windows_commands, windows_list};
use std::sync::{Mutex, OnceLock};
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
            heading: "Windows".to_string(),
            title_template: "{marker} {index}{name}".to_string(),
            description_template: String::new(),
            status_template: String::new(),
            maximum_visible_items: 20,
            content_height: false,
            collapse_below_width: 80,
            collapsed_width: 8,
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

#[derive(Debug)]
struct CompanionState {
    // None selects the legacy process-local presentation until runtime ownership
    // is threaded through installation; explicit owners never publish globally.
    surfaces: Option<std::sync::Arc<bmux_plugin::surface::PluginSurfaceRegistry>>,
    subscription: Option<tokio::task::JoinHandle<()>>,
    allocation: Option<ExtensionRect>,
    allocation_publication_pending: bool,
    settings: Settings,
    revision: u64,
    snapshot: windows_list::WindowListSnapshot,
    hovered_window_id: Option<Uuid>,
    scroll: bmux_tui_components::scroll_view::ScrollViewState,
    measured_items: std::cell::RefCell<std::collections::BTreeMap<Uuid, MeasuredSidebarItem>>,
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
    const fn new(settings: Settings) -> Self {
        Self {
            surfaces: None,
            subscription: None,
            allocation: None,
            allocation_publication_pending: false,
            settings,
            revision: 0,
            snapshot: windows_list::WindowListSnapshot {
                windows: Vec::new(),
                revision: 0,
            },
            hovered_window_id: None,
            scroll: bmux_tui_components::scroll_view::ScrollViewState::new(),
            measured_items: std::cell::RefCell::new(std::collections::BTreeMap::new()),
        }
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
        if let Some(index) = self
            .snapshot
            .windows
            .iter()
            .position(|window| window.active)
        {
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
        publish(self)?;
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
                .fold(Column::new().id(format!("window:{id}")), |column, field| {
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

    fn measured_window(&self, index: usize) -> std::cell::Ref<'_, MeasuredSidebarItem> {
        let window = &self.snapshot.windows[index];
        let fact = window_fact(window);
        let render = |template: &str| {
            render_template(
                template,
                window,
                index,
                self.settings.show_index,
                fact.as_ref(),
            )
        };
        self.measure_item(
            window.id,
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
        for index in 0..self.snapshot.windows.len() {
            let item = self.measured_window(index);
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
            usize::try_from(item.y).unwrap_or(usize::MAX),
            usize::try_from(item.node.size.height).unwrap_or(usize::MAX),
        );
    }

    fn replace_windows(&mut self, snapshot: windows_list::WindowListSnapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            self.measured_items
                .get_mut()
                .retain(|id, _| self.snapshot.windows.iter().any(|window| window.id == *id));
            let layout = self.scroll_layout();
            bmux_tui_components::scroll_view::ScrollView::scroll_vertical_by(
                &layout,
                &mut self.scroll,
                0,
            );
            if let Some(active) = self
                .snapshot
                .windows
                .iter()
                .position(|window| window.active)
            {
                self.reveal(active);
            }
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }
}

type CompanionHandle = std::sync::Arc<Mutex<Option<CompanionState>>>;

fn installations() -> &'static Mutex<CompanionHandle> {
    static STATE: OnceLock<Mutex<CompanionHandle>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(std::sync::Arc::new(Mutex::new(None))))
}

fn state() -> CompanionHandle {
    installations()
        .lock()
        .expect("sidebar installation lock poisoned")
        .clone()
}

fn replace_installation(settings: Settings) -> Result<CompanionHandle, String> {
    let next = std::sync::Arc::new(Mutex::new(Some(CompanionState::new(settings))));
    let mut current = installations()
        .lock()
        .map_err(|_| "sidebar installation lock poisoned".to_string())?;
    *current
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())? = None;
    *current = next.clone();
    drop(current);
    Ok(next)
}

#[derive(Default)]
pub struct SidebarPlugin;

impl RustPlugin for SidebarPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let settings = Settings::parse(context.settings.as_ref())?;
        replace_installation(settings).map_err(PluginCommandError::failed)?;
        Ok(EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        uninstall();
        Ok(EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "presentation-input", "handle-input" => |event: AttachInputEvent, ctx| {
                Ok::<_, ServiceResponse>(handle_input(ctx, &event))
            },
        })
    }
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
    /// Install into presentation-local registries.
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
        let settings = Settings::parse(settings).map_err(|error| error.to_string())?;
        let request = layout_request(&settings);
        let mut companion = CompanionState::new(settings);
        companion.surfaces = Some(surfaces);
        let owner = std::sync::Arc::new(Mutex::new(Some(companion)));
        layouts
            .publish(
                OWNER,
                PluginLayoutSnapshot {
                    revision: 1,
                    requests: vec![request],
                },
            )
            .map_err(|error| format!("publishing sidebar layout: {error:?}"))?;
        register_input_callbacks(
            &owner,
            &|id, handler| allocations.register(id, handler),
            &|endpoint, handler| input.register(endpoint, handler),
        );
        Ok(Self {
            owner,
            layouts,
            allocations,
            input,
        })
    }

    /// Subscribe this presentation to its host's authoritative state bus.
    /// Replaces any previous subscription; teardown aborts the owned task.
    ///
    /// # Errors
    /// Returns subscription, publication, or task startup errors.
    pub fn start(&self, bus: &bmux_plugin::EventBus) -> Result<(), String> {
        start_for_installation(&self.owner, bus)
    }

    /// Apply the authoritative snapshot for this presentation.
    ///
    /// # Errors
    /// Returns state-lock or surface-publication errors.
    pub fn publish(&self, snapshot: windows_list::WindowListSnapshot) -> Result<(), String> {
        publish_for_installation(&self.owner, snapshot)
    }
}

impl Drop for SidebarPresentation {
    fn drop(&mut self) {
        self.allocations
            .remove(&PluginLayoutId::new(OWNER, LAYOUT_ID));
        self.input.remove(&input_endpoint());
        if let Ok(mut owner) = self.owner.lock() {
            *owner = None;
        }
        self.layouts.remove_owner(OWNER);
    }
}

fn default_presentation() -> &'static Mutex<Option<SidebarPresentation>> {
    static PRESENTATION: OnceLock<Mutex<Option<SidebarPresentation>>> = OnceLock::new();
    PRESENTATION.get_or_init(|| Mutex::new(None))
}

/// Install the default process-local presentation.
///
/// # Errors
/// Returns configuration, registry publication, or lock errors.
pub fn install(settings: Option<&toml::Value>) -> Result<(), String> {
    // Reject invalid configuration before tearing down a working installation.
    Settings::parse(settings).map_err(|error| error.to_string())?;
    let mut current = default_presentation()
        .lock()
        .map_err(|_| "sidebar presentation lock poisoned".to_string())?;
    *current = None;
    let presentation = SidebarPresentation::install(
        settings,
        bmux_plugin::layout::global_plugin_layout_registry_handle(),
        bmux_plugin::layout::global_allocation_registry_handle(),
        bmux_plugin::global_attach_presentation_input_registry_handle(),
        bmux_plugin::surface::global_plugin_surface_registry_handle(),
    )?;
    let mut installed = installations()
        .lock()
        .map_err(|_| "sidebar installation lock poisoned".to_string())?;
    *installed
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())? = None;
    *installed = presentation.owner.clone();
    drop(installed);
    *current = Some(presentation);
    drop(current);
    Ok(())
}

/// Capture lifecycle callbacks for the current installation.
///
/// # Errors
/// Returns an error if no presentation is installed or its lock is poisoned.
pub fn installed_companion() -> Result<bmux_plugin::AttachCompanion, String> {
    let current = default_presentation()
        .lock()
        .map_err(|_| "sidebar presentation lock poisoned".to_string())?;
    let owner = current
        .as_ref()
        .ok_or_else(|| "sidebar companion is not installed".to_string())?
        .owner
        .clone();
    drop(current);
    let start_owner = owner.clone();
    Ok(bmux_plugin::AttachCompanion::new(
        OWNER,
        std::sync::Arc::new(move || {
            start_for_installation(&start_owner, &bmux_plugin::global_event_bus())
        }),
        std::sync::Arc::new(move || {
            let mut current = default_presentation()
                .lock()
                .map_err(|_| "sidebar presentation lock poisoned".to_string())?;
            // A stale runtime must not remove a newer installation's registrations.
            if current
                .as_ref()
                .is_some_and(|presentation| std::sync::Arc::ptr_eq(&presentation.owner, &owner))
            {
                *current = None;
            }
            drop(current);
            Ok(())
        }),
    ))
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
            if let Some(companion) = guard.as_mut()
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

/// Subscribe the configured companion to authoritative window state.
///
/// # Errors
///
/// Returns an error when state subscription, initial publication, or task startup fails.
pub fn start() -> Result<(), String> {
    let current = default_presentation()
        .lock()
        .map_err(|_| "sidebar presentation lock poisoned".to_string())?;
    current
        .as_ref()
        .ok_or_else(|| "sidebar companion is not installed".to_string())?
        .start(&bmux_plugin::global_event_bus())
}

fn start_for_installation(
    owner: &CompanionHandle,
    bus: &bmux_plugin::EventBus,
) -> Result<(), String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("sidebar companion requires an async runtime: {error}"))?;
    let (initial, mut receiver) = bus
        .subscribe_state::<windows_list::WindowListSnapshot>(&windows_list::STATE_KIND)
        .map_err(|error| format!("subscribing to windows list: {error}"))?;
    publish_for_installation(owner, initial.as_ref().clone())?;
    let mut guard = owner
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())?;
    let companion = guard
        .as_mut()
        .ok_or_else(|| "sidebar companion is not installed".to_string())?;
    if let Some(previous) = companion.subscription.take() {
        previous.abort();
    }
    let subscriber_owner = std::sync::Arc::downgrade(owner);
    companion.subscription = Some(handle.spawn(async move {
        while receiver.changed().await.is_ok() {
            let snapshot = receiver.borrow_and_update().as_ref().clone();
            let Some(owner) = subscriber_owner.upgrade() else {
                break;
            };
            if let Err(error) = publish_for_installation(&owner, snapshot) {
                tracing::warn!(%error, "sidebar publication failed");
            }
        }
    }));
    drop(guard);
    Ok(())
}

pub fn uninstall() {
    if let Ok(mut current) = default_presentation().lock() {
        *current = None;
    }
    bmux_plugin::layout::remove_allocation_handler(&PluginLayoutId::new(OWNER, LAYOUT_ID));
    bmux_plugin::remove_attach_presentation_input_handler(&input_endpoint());
    let _ = global_plugin_layout_registry().remove_owner(OWNER);
    let _ = global_plugin_surface_registry().remove_owner(OWNER);
    if let Ok(mut guard) = state().lock() {
        *guard = None;
    }
}

fn layout_request(settings: &Settings) -> PluginLayoutRequest {
    PluginLayoutRequest::split(
        PluginLayoutId::new(OWNER, LAYOUT_ID),
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

#[cfg(test)]
fn publish(snapshot: windows_list::WindowListSnapshot) -> Result<(), String> {
    publish_for_installation(&state(), snapshot)
}

fn publish_for_installation(
    owner: &CompanionHandle,
    snapshot: windows_list::WindowListSnapshot,
) -> Result<(), String> {
    let mut guard = owner
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())?;
    if let Some(companion) = guard.as_mut() {
        companion.replace_windows(snapshot);
        return publish_companion(companion);
    }
    drop(guard);
    Ok(())
}

fn publish_surface(
    registry: &bmux_plugin::surface::PluginSurfaceRegistry,
    revision: u64,
    surface: &PluginSurface,
) -> Result<(), String> {
    registry
        .publish_advancing(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface.clone()],
            },
        )
        .map_err(|error| format!("publishing sidebar surface: {error:?}"))?;
    Ok(())
}

fn publish_companion(companion: &CompanionState) -> Result<(), String> {
    let revision = companion.revision.max(1);
    let surface = build_surface(companion, revision);
    let registry = companion
        .surfaces
        .as_deref()
        .unwrap_or_else(|| global_plugin_surface_registry());
    publish_surface(registry, revision, &surface)
}

fn truncate_to_width(value: &str, maximum: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;

    let mut result = String::new();
    let mut width = 0_usize;
    for grapheme in value.graphemes(true) {
        let cell_width = unicode_width::UnicodeWidthStr::width(grapheme);
        if width.saturating_add(cell_width) > maximum {
            break;
        }
        result.push_str(grapheme);
        width = width.saturating_add(cell_width);
    }
    result
}

fn window_fact(window: &windows_list::WindowListEntry) -> Option<PresentationFact> {
    let entity = PresentationEntityRef::new("bmux.windows", window.id.to_string());
    global_presentation_fact_host_service()
        .registry()
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
    window: &windows_list::WindowListEntry,
    index: usize,
    show_index: bool,
    fact: Option<&PresentationFact>,
) -> String {
    const MARKER_TOKEN: &str = concat!("{", "marker}");
    const INDEX_TOKEN: &str = concat!("{", "index}");
    const FACT_TOKEN: &str = concat!("{", "fact}");
    let marker = if window.active { "●" } else { "○" };
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
        .replace("{name}", &window.name)
        .replace("{id}", &window.id.to_string())
        .replace("{active}", if window.active { "active" } else { "idle" })
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
    placement: &(usize, std::ops::Range<usize>),
    style: RenderStyle,
    ops: &mut Vec<RenderOp>,
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
    let height = u16::try_from(child.node.size.height).unwrap_or(u16::MAX);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    field.paint(&child.node, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
    for (offset, cells) in buffer.cells().chunks(usize::from(width)).enumerate() {
        let text = cells
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        let logical_row = placement
            .0
            .saturating_add(usize::try_from(child.y).unwrap_or(usize::MAX))
            .saturating_add(offset);
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

fn sidebar_region(id: Uuid, width: u16, row: u16, height: u16) -> PluginSurfaceRegion {
    PluginSurfaceRegion::new(
        format!("window:{id}"),
        ExtensionRect::new(1, row, width.saturating_sub(2), height.max(1)),
    )
    .endpoint(input_endpoint())
    .focusable(bmux_plugin::surface::PluginSurfaceCursor::Pointer)
}

fn build_surface(state: &CompanionState, revision: u64) -> PluginSurface {
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
    let mut regions = Vec::with_capacity(state.snapshot.windows.len());
    let layout = state.scroll_layout();
    let visible = state.scroll.vertical_offset()
        ..state
            .scroll
            .vertical_offset()
            .saturating_add(usize::try_from(layout.size.height).unwrap_or(usize::MAX));
    let mut row = 1_u16;
    for (index, window) in state.snapshot.windows.iter().enumerate() {
        let item = &layout.children[0].node.children[index];
        let item_end = item.y.saturating_add(item.node.size.height);
        let Some(projected) = bmux_tui_components::scroll_view::ScrollView::project_rows(
            &visible,
            usize::try_from(item.y).unwrap_or(usize::MAX)
                ..usize::try_from(item_end).unwrap_or(usize::MAX),
        ) else {
            continue;
        };
        let fact = window_fact(window);
        let start_row = u16::try_from(projected.start + 1).unwrap_or(u16::MAX);
        let measured = state.measured_window(index);
        let item_style = fact.as_ref().map_or_else(
            || {
                sidebar_title_style(
                    window.active,
                    state.hovered_window_id == Some(window.id),
                    active,
                    inactive,
                )
            },
            |fact| fact_style(fact.role, if window.active { active } else { inactive }),
        );
        for (field, child) in measured.fields.iter().zip(&measured.layout.children) {
            let style = match child.node.id.as_str() {
                "title" => item_style,
                "status" if window.active => active,
                _ => inactive.dim(),
            };
            paint_sidebar_field(
                field,
                child,
                &(
                    usize::try_from(item.y).unwrap_or(usize::MAX),
                    visible.clone(),
                ),
                style,
                &mut ops,
            );
        }
        row = u16::try_from(projected.end + 1).unwrap_or(u16::MAX);
        regions.push(sidebar_region(
            window.id,
            width,
            start_row,
            row.saturating_sub(start_row),
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
    for region in regions {
        surface = surface.interactive_region(region);
    }
    surface
}

fn update_hover(owner: &CompanionHandle, event: &AttachInputEvent) -> bool {
    let target = event
        .hook_id
        .strip_prefix("bmux.sidebar:sidebar:window:")
        .and_then(|target| Uuid::parse_str(target).ok());
    let hovered = match event.phase.as_str() {
        "enter" | "move" => target,
        "leave" => None,
        _ => return false,
    };
    let Ok(mut guard) = owner.lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    if companion.hovered_window_id == hovered {
        return false;
    }
    companion.hovered_window_id = hovered;
    companion.revision = companion.revision.saturating_add(1).max(1);
    publish_companion(companion).is_ok()
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
    publish_companion(companion).is_ok()
}

fn update_keyboard(owner: &CompanionHandle, event: &AttachInputEvent) -> Option<AttachInputResult> {
    if event.event_kind != "key" || !matches!(event.phase.as_str(), "press" | "repeat") {
        return None;
    }
    let target = event
        .hook_id
        .strip_prefix("bmux.sidebar:sidebar:window:")
        .and_then(|target| Uuid::parse_str(target).ok())?;
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    let index = companion
        .snapshot
        .windows
        .iter()
        .position(|window| window.id == target)?;
    match event.key.as_deref()? {
        "up" => {
            let next = index.saturating_sub(1);
            companion.reveal(next);
            companion.hovered_window_id =
                companion.snapshot.windows.get(next).map(|window| window.id);
        }
        "down" => {
            let next = index
                .saturating_add(1)
                .min(companion.snapshot.windows.len().saturating_sub(1));
            companion.reveal(next);
            companion.hovered_window_id =
                companion.snapshot.windows.get(next).map(|window| window.id);
        }
        _ => return None,
    }
    let dirty = {
        companion.revision = companion.revision.saturating_add(1).max(1);
        publish_companion(companion).is_ok()
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
    let Some(target) = event.hook_id.strip_prefix("bmux.sidebar:sidebar:window:") else {
        return AttachInputResult::default();
    };
    let mut client = ServiceCallerDispatchClient::new(context);
    match block_on_typed_dispatch(windows_commands::client::switch_window(
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
            status_message: Some(format!("window switch failed: {error:?}")),
            ..AttachInputResult::default()
        },
        Err(error) => AttachInputResult {
            consumed: true,
            status_message: Some(format!("window switch unavailable: {error}")),
            ..AttachInputResult::default()
        },
    }
}

bmux_plugin_sdk::export_plugin!(SidebarPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn republish_advances_past_retained_owner_revision() {
        uninstall();
        install(None).expect("install sidebar");
        publish(windows_list::WindowListSnapshot {
            windows: Vec::new(),
            revision: 0,
        })
        .expect("initial publish");
        global_plugin_surface_registry()
            .publish(
                OWNER,
                PluginSurfaceSnapshot {
                    revision: 9,
                    surfaces: Vec::new(),
                },
            )
            .expect("seed higher owner revision");

        publish(windows_list::WindowListSnapshot {
            windows: Vec::new(),
            revision: 1,
        })
        .expect("republish above retained revision");
        assert_eq!(
            global_plugin_surface_registry()
                .owner_snapshot(OWNER)
                .expect("owner snapshot")
                .revision,
            10
        );
        let foreign_identity = std::sync::Arc::new(Mutex::new(None));
        publish_for_installation(
            &foreign_identity,
            windows_list::WindowListSnapshot {
                windows: Vec::new(),
                revision: 99,
            },
        )
        .unwrap();
        assert_eq!(
            global_plugin_surface_registry()
                .owner_snapshot(OWNER)
                .unwrap()
                .revision,
            10
        );
        let old_companion = installed_companion().unwrap();
        install(None).unwrap();
        let replacement = state();
        old_companion.stop().unwrap();
        assert!(replacement.lock().unwrap().is_some());
        assert!(
            global_plugin_layout_registry()
                .requests()
                .iter()
                .any(|request| request.id.owner_plugin_id == OWNER)
        );
        installed_companion().unwrap().stop().unwrap();
        assert!(replacement.lock().unwrap().is_none());
        uninstall();
    }

    #[test]
    fn layout_request_places_sidebar_on_configured_edge_and_order() {
        let left = layout_request(&Settings::default());
        let right = layout_request(&Settings {
            placement: Placement::Right,
            order: 50,
            ..Settings::default()
        });
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
    fn templates_escape_braces_and_expand_stable_window_fields() {
        let window = windows_list::WindowListEntry {
            id: Uuid::from_u128(10),
            name: "build".to_string(),
            active: true,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        assert_eq!(
            render_template("{{literal}} {index}{name} {active}", &window, 1, true, None,),
            "{literal} 2 build active"
        );
    }

    #[test]
    fn semantic_fact_populates_templates_and_style_role() {
        let window = windows_list::WindowListEntry {
            id: Uuid::from_u128(20),
            name: "build".to_string(),
            active: false,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        let fact = PresentationFact {
            entity: PresentationEntityRef::new("bmux.windows", window.id.to_string()),
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
                &window,
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
        let mut state = CompanionState::new(Settings::default());
        let first = Uuid::from_u128(11);
        let second = Uuid::from_u128(12);
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: vec![
                windows_list::WindowListEntry {
                    id: first,
                    name: "one".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
                windows_list::WindowListEntry {
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
        state.hovered_window_id = Some(second);
        let after = build_surface(&state, 2);
        assert_eq!(before.ops[3], after.ops[3]);
        assert_ne!(before.ops[4], after.ops[4]);
    }

    #[tokio::test]
    async fn presentation_subscription_uses_local_bus_and_stops_on_drop() {
        let bus = bmux_plugin::EventBus::new();
        let snapshot = |revision| windows_list::WindowListSnapshot {
            windows: Vec::new(),
            revision,
        };
        bus.register_state_channel(windows_list::STATE_KIND, snapshot(1));
        let presentation = SidebarPresentation::install(
            None,
            std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4)),
            std::sync::Arc::new(bmux_plugin::layout::AllocationRegistry::default()),
            std::sync::Arc::new(bmux_plugin::AttachPresentationInputRegistry::new()),
            std::sync::Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(4)),
        )
        .unwrap();
        presentation.start(&bus).unwrap();
        assert_eq!(
            presentation
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .snapshot
                .revision,
            1
        );
        bus.publish_state(&windows_list::STATE_KIND, snapshot(2))
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if presentation
                    .owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .snapshot
                    .revision
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let owner = presentation.owner.clone();
        drop(presentation);
        bus.publish_state(&windows_list::STATE_KIND, snapshot(3))
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
                None,
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
        assert!(first_layouts.requests().is_empty());
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
        let mut first = CompanionState::new(Settings::default());
        first.surfaces = Some(first_registry.clone());
        let mut second = CompanionState::new(Settings::default());
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
            let mut companion = CompanionState::new(Settings::default());
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
        let mut state = CompanionState::new(Settings::default());
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
    fn measured_scroll_clips_partial_items_and_reveals_full_extent() {
        let mut state = CompanionState::new(Settings {
            maximum_visible_items: 1,
            description_template: "description".to_string(),
            ..Settings::default()
        });
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: (0..3)
                .map(|index| windows_list::WindowListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("window-{index}"),
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
        assert!(build_surface(&state, 3).interactive_regions.is_empty());
    }

    #[test]
    fn virtual_window_realigns_to_active_and_bounds_projected_items() {
        let settings = Settings {
            maximum_visible_items: 2,
            ..Settings::default()
        };
        let mut state = CompanionState::new(settings);
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: (0..5)
                .map(|index| windows_list::WindowListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("window-{index}"),
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
            format!("window:{}", Uuid::from_u128(4))
        );
    }

    #[test]
    fn one_semantic_fact_changes_only_its_target_card_operation() {
        let producer = format!("sidebar-test-{}", Uuid::from_u128(30));
        let first = Uuid::from_u128(31);
        let second = Uuid::from_u128(32);
        let mut state = CompanionState::new(Settings::default());
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: vec![
                windows_list::WindowListEntry {
                    id: first,
                    name: "one".to_string(),
                    active: false,
                    workspace: "default".to_string(),
                    workspace_id: uuid::Uuid::nil(),
                },
                windows_list::WindowListEntry {
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
        global_presentation_fact_host_service()
            .registry()
            .publish(
                &producer,
                bmux_presentation_state::PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![PresentationFact {
                        entity: PresentationEntityRef::new("bmux.windows", second.to_string()),
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
        assert!(
            global_presentation_fact_host_service()
                .registry()
                .remove_producer(&producer)
        );
    }

    #[test]
    #[ignore = "manual presentation performance baseline; run with --release --ignored --nocapture"]
    fn sidebar_projection_performance_baseline() {
        use std::time::Instant;

        let settings = Settings {
            maximum_visible_items: 32,
            ..Settings::default()
        };
        let mut state = CompanionState::new(settings);
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: (0..2_000)
                .map(|index| windows_list::WindowListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("window-{index}"),
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
        eprintln!("sidebar 2,000-window/32-visible projection average: {average_ns} ns");
        assert!(average_ns < 70_000, "projection exceeded 70 us budget");
    }

    #[test]
    fn single_and_large_window_lists_remain_bounded() {
        let settings = Settings {
            maximum_visible_items: 32,
            ..Settings::default()
        };
        let mut state = CompanionState::new(settings);
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: vec![windows_list::WindowListEntry {
                id: Uuid::from_u128(50),
                name: "single".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        assert_eq!(build_surface(&state, 1).interactive_regions.len(), 1);

        state.replace_windows(windows_list::WindowListSnapshot {
            windows: (0..2_000)
                .map(|index| windows_list::WindowListEntry {
                    id: Uuid::from_u128(index),
                    name: format!("window-{index}"),
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
        let mut state = CompanionState::new(Settings {
            content_height: true,
            ..Settings::default()
        });
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: vec![windows_list::WindowListEntry {
                id: Uuid::from_u128(40),
                name: "one".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        assert!(matches!(
            surface.ops[0],
            RenderOp::FillRect {
                rect: ExtensionRect { h: 3, .. },
                ..
            }
        ));
    }

    #[test]
    fn surface_uses_one_stable_region_per_window() {
        let mut state = CompanionState::new(Settings::default());
        let id = Uuid::from_u128(9);
        state.replace_windows(windows_list::WindowListSnapshot {
            windows: vec![windows_list::WindowListEntry {
                id,
                name: "main".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            }],
            revision: 1,
        });
        let surface = build_surface(&state, 1);
        assert_eq!(
            surface.interactive_regions[0].local_id,
            format!("window:{id}")
        );
        assert!(surface.accepts_input);
    }
}
