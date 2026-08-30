#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

mod settings;

use bmux_attach_view_protocol::AttachLocalPresentationSnapshot;
use bmux_plugin::layout::{
    LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest, PluginLayoutSnapshot,
    global_plugin_layout_registry,
};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceSnapshot,
    global_plugin_surface_registry,
};
use bmux_plugin::{
    AttachInputEvent, AttachInputResult, ExtensionRect, RenderNamedColor, RenderOp, RenderStyle,
    ServiceCallerDispatchClient, block_on_typed_dispatch,
};
use bmux_plugin_sdk::prelude::*;
use bmux_presentation_state::{
    PresentationEntityRef, PresentationFact, PresentationFactRole,
    global_presentation_fact_host_service,
};
use bmux_tui_components::tab_bar::TabItem;
use bmux_windows_plugin_api::{windows_commands, windows_list};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const OWNER: &str = "bmux.tab_strip";
const LAYOUT_ID: &str = "strip";
const SURFACE_ID: &str = "strip";
const RETAINED_ID: Uuid = Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0001);
const ATTACH_LOCAL_PRESENTATION_STATE_KIND: bmux_plugin_sdk::PluginEventKind =
    bmux_plugin_sdk::PluginEventKind::from_static("bmux.attach/local-presentation");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    TabRail,
    Minimal,
    Classic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Density {
    Compact,
    Cozy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverflowStyle {
    Count,
    Arrows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveAlignment {
    KeepVisible,
    FocusBias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeparatorSet {
    AngledSegments,
    Plain,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabScope {
    AllContexts,
    SessionContexts,
    Mru,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabOrder {
    Stable,
    Mru,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintPolicy {
    Always,
    ScrollOnly,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ColorSettings {
    bar_bg: Option<String>,
    bar_fg: Option<String>,
    tab_active_bg: Option<String>,
    tab_active_fg: Option<String>,
    tab_inactive_bg: Option<String>,
    tab_inactive_fg: Option<String>,
    tab_hover_bg: Option<String>,
    tab_hover_fg: Option<String>,
    tab_active_hover_bg: Option<String>,
    tab_active_hover_fg: Option<String>,
    module_bg: Option<String>,
    module_fg: Option<String>,
    overflow_bg: Option<String>,
    overflow_fg: Option<String>,
}

#[allow(clippy::struct_excessive_bools)] // Independent legacy-compatible visibility and emphasis settings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    placement: Placement,
    height: u16,
    order: i32,
    preset: Preset,
    density: Density,
    left_padding: usize,
    right_padding: usize,
    tab_gap: usize,
    module_gap: usize,
    overflow_style: OverflowStyle,
    align_active: ActiveAlignment,
    separator_set: SeparatorSet,
    prefer_unicode: bool,
    force_ascii: bool,
    dim_inactive: bool,
    bold_active: bool,
    underline_active: bool,
    maximum_visible_tabs: Option<usize>,
    maximum_label_width: u16,
    label_template: String,
    tab_scope: TabScope,
    tab_order: TabOrder,
    show_session_name: bool,
    show_context_name: bool,
    show_mode: bool,
    show_role: bool,
    show_follow: bool,
    show_hint: bool,
    hover_highlight: bool,
    hint_policy: HintPolicy,
    show_compact_facts: bool,
    colors: ColorSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            placement: Placement::Bottom,
            height: 1,
            order: 100,
            preset: Preset::TabRail,
            density: Density::Cozy,
            left_padding: 1,
            right_padding: 1,
            tab_gap: 1,
            module_gap: 1,
            overflow_style: OverflowStyle::Arrows,
            align_active: ActiveAlignment::KeepVisible,
            separator_set: SeparatorSet::AngledSegments,
            prefer_unicode: true,
            force_ascii: false,
            dim_inactive: true,
            bold_active: true,
            underline_active: false,
            maximum_visible_tabs: None,
            maximum_label_width: 20,
            label_template: "{name}".to_string(),
            tab_scope: TabScope::AllContexts,
            tab_order: TabOrder::Stable,
            show_session_name: false,
            show_context_name: false,
            show_mode: true,
            show_role: true,
            show_follow: true,
            show_hint: true,
            hover_highlight: true,
            hint_policy: HintPolicy::ScrollOnly,
            show_compact_facts: false,
            colors: ColorSettings::default(),
        }
    }
}

impl Settings {
    fn parse(value: Option<&toml::Value>) -> Result<Self, PluginCommandError> {
        settings::parse_settings(value)
    }
}

#[derive(Debug, Clone)]
struct CompanionState {
    settings: Settings,
    revision: u64,
    snapshot: windows_list::WindowListSnapshot,
    hovered_window_id: Option<Uuid>,
    scroll_offset: usize,
    pointer_source: Option<Uuid>,
    pointer_moved: bool,
    editing_window_id: Option<Uuid>,
    edit_buffer: String,
    menu_window_id: Option<Uuid>,
    menu_selected: usize,
    local_presentation: AttachLocalPresentationSnapshot,
}

impl CompanionState {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            revision: 0,
            snapshot: windows_list::WindowListSnapshot {
                windows: Vec::new(),
                revision: 0,
            },
            hovered_window_id: None,
            scroll_offset: 0,
            pointer_source: None,
            pointer_moved: false,
            editing_window_id: None,
            edit_buffer: String::new(),
            menu_window_id: None,
            menu_selected: 0,
            local_presentation: AttachLocalPresentationSnapshot::initial(),
        }
    }

    fn visible_limit(&self) -> usize {
        self.settings.maximum_visible_tabs.unwrap_or(usize::MAX)
    }

    fn replace_windows(&mut self, snapshot: windows_list::WindowListSnapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            let visible_limit = self.visible_limit();
            let maximum_offset = self.snapshot.windows.len().saturating_sub(visible_limit);
            self.scroll_offset = self.scroll_offset.min(maximum_offset);
            if let Some(active) = self
                .snapshot
                .windows
                .iter()
                .position(|window| window.active)
            {
                if active < self.scroll_offset {
                    self.scroll_offset = active;
                } else if active >= self.scroll_offset.saturating_add(visible_limit) {
                    self.scroll_offset = active.saturating_add(1).saturating_sub(visible_limit);
                }
            }
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }

    fn replace_local_presentation(&mut self, snapshot: AttachLocalPresentationSnapshot) {
        if self.local_presentation != snapshot {
            self.local_presentation = snapshot;
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }
}

fn state() -> &'static Mutex<Option<CompanionState>> {
    static STATE: OnceLock<Mutex<Option<CompanionState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

#[derive(Default)]
pub struct TabStripPlugin;

impl RustPlugin for TabStripPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let settings = Settings::parse(context.settings.as_ref())?;
        *state()
            .lock()
            .map_err(|_| PluginCommandError::failed("tab-strip state lock poisoned"))? =
            Some(CompanionState::new(settings));
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

/// Configure this process-local attach companion.
///
/// # Errors
///
/// Returns an error for invalid settings or rejected retained publication.
pub fn install(settings: Option<&toml::Value>) -> Result<(), String> {
    let settings = Settings::parse(settings).map_err(|error| error.to_string())?;
    let request = layout_request(&settings);
    let mut guard = state()
        .lock()
        .map_err(|_| "tab-strip state lock poisoned".to_string())?;
    *guard = Some(CompanionState::new(settings));
    drop(guard);

    let _ = global_plugin_layout_registry().remove_owner(OWNER);
    let _ = global_plugin_surface_registry().remove_owner(OWNER);
    global_plugin_layout_registry()
        .publish(
            OWNER,
            PluginLayoutSnapshot {
                revision: 1,
                requests: vec![request],
            },
        )
        .map_err(|error| format!("publishing tab-strip layout: {error:?}"))?;
    Ok(())
}

/// Subscribe the configured companion to authoritative window state.
///
/// # Errors
///
/// Returns an error when state subscription, initial publication, or task startup fails.
pub fn start() -> Result<(), String> {
    let (initial, mut receiver) = bmux_plugin::global_event_bus()
        .subscribe_state::<windows_list::WindowListSnapshot>(&windows_list::STATE_KIND)
        .map_err(|error| format!("subscribing to windows list: {error}"))?;
    let (initial_local_presentation, mut local_presentation_receiver) =
        bmux_plugin::global_event_bus()
            .subscribe_state::<AttachLocalPresentationSnapshot>(
                &ATTACH_LOCAL_PRESENTATION_STATE_KIND,
            )
            .map_err(|error| format!("subscribing to attach-local presentation: {error}"))?;
    publish(initial.as_ref().clone())?;
    publish_local_presentation(initial_local_presentation.as_ref().clone())?;
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("tab-strip companion requires an async runtime: {error}"))?;
    handle.spawn(async move {
        while receiver.changed().await.is_ok() {
            let snapshot = receiver.borrow_and_update().as_ref().clone();
            if let Err(error) = publish(snapshot) {
                tracing::warn!(%error, "tab-strip publication failed");
            }
        }
    });
    handle.spawn(async move {
        while local_presentation_receiver.changed().await.is_ok() {
            let snapshot = local_presentation_receiver
                .borrow_and_update()
                .as_ref()
                .clone();
            if let Err(error) = publish_local_presentation(snapshot) {
                tracing::warn!(%error, "tab-strip local presentation publication failed");
            }
        }
    });
    Ok(())
}

pub fn uninstall() {
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
            Placement::Top => LayoutEdge::Top,
            Placement::Bottom => LayoutEdge::Bottom,
        },
        LayoutExtent::Cells(settings.height),
    )
}

// The companion lock must cover registry publication so multiple retained-state
// subscriber tasks cannot race the same owner revision.
#[allow(clippy::significant_drop_tightening)]
fn publish(snapshot: windows_list::WindowListSnapshot) -> Result<(), String> {
    let mut guard = state()
        .lock()
        .map_err(|_| "tab-strip state lock poisoned".to_string())?;
    let Some(companion) = guard.as_mut() else {
        return Ok(());
    };
    companion.replace_windows(snapshot);
    let revision = companion.revision.max(1);
    let surface = build_surface(companion, revision);
    publish_surface(revision, &surface)
}

#[allow(clippy::significant_drop_tightening)] // The state lock must cover projection so snapshot fields remain coherent.
fn publish_local_presentation(snapshot: AttachLocalPresentationSnapshot) -> Result<(), String> {
    let (revision, surface) = {
        let mut guard = state()
            .lock()
            .map_err(|_| "tab-strip state lock poisoned".to_string())?;
        let Some(companion) = guard.as_mut() else {
            return Ok(());
        };
        companion.replace_local_presentation(snapshot);
        let revision = companion.revision.max(1);
        (revision, build_surface(companion, revision))
    };
    publish_surface(revision, &surface)
}

fn publish_surface(revision: u64, surface: &PluginSurface) -> Result<(), String> {
    global_plugin_surface_registry()
        .publish_advancing(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface.clone()],
            },
        )
        .map_err(|error| format!("publishing tab-strip surface: {error:?}"))?;
    Ok(())
}

fn publish_companion(companion: &CompanionState) -> Result<(), String> {
    let revision = companion.revision.max(1);
    let surface = build_surface(companion, revision);
    publish_surface(revision, &surface)
}

fn compact_fact(window: &windows_list::WindowListEntry) -> Option<PresentationFact> {
    let entity = PresentationEntityRef::new("bmux.windows", window.id.to_string());
    global_presentation_fact_host_service()
        .registry()
        .facts_for_entity(&entity)
        .into_iter()
        .map(|(_, fact)| fact)
        .max_by(|left, right| {
            (left.priority, compact_role_rank(left.role), &left.key).cmp(&(
                right.priority,
                compact_role_rank(right.role),
                &right.key,
            ))
        })
}

const fn compact_role_rank(role: PresentationFactRole) -> u8 {
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

const fn compact_fact_style(role: PresentationFactRole, fallback: RenderStyle) -> RenderStyle {
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

fn component_label(id: &str, label: &str) -> String {
    TabItem::new(id, label).label()
}

fn tab_label(settings: &Settings, window: &windows_list::WindowListEntry, index: usize) -> String {
    const INDEX_TOKEN: &str = concat!("{", "index}");
    const INDEX0_TOKEN: &str = concat!("{", "index0}");
    const SESSION_TOKEN: &str = concat!("{", "session}");
    const MARKER_TOKEN: &str = concat!("{", "marker}");
    const FACT_TOKEN: &str = concat!("{", "fact}");
    let display_index = index.saturating_add(1).to_string();
    let zero_index = index.to_string();
    let marker = if window.active { "*" } else { "" };
    let session = "";
    let fact = settings
        .show_compact_facts
        .then(|| compact_fact(window))
        .flatten();
    let label = settings
        .label_template
        .replace("{{", "\u{0}")
        .replace("}}", "\u{1}")
        .replace(INDEX_TOKEN, &display_index)
        .replace(INDEX0_TOKEN, &zero_index)
        .replace("{name}", &window.name)
        .replace(SESSION_TOKEN, session)
        .replace(MARKER_TOKEN, marker)
        .replace("{id}", &window.id.to_string())
        .replace("{active}", if window.active { "active" } else { "idle" })
        .replace(
            FACT_TOKEN,
            fact.as_ref().map_or("", |fact| fact.short_text.as_str()),
        )
        .replace('\u{0}', "{")
        .replace('\u{1}', "}");
    truncate_to_width(
        &component_label(&window.id.to_string(), &label),
        usize::from(settings.maximum_label_width),
    )
}

#[allow(clippy::too_many_lines)] // One ordered retained projection keeps tab, overflow, editor, and menu geometry consistent.
fn build_surface(state: &CompanionState, revision: u64) -> PluginSurface {
    let active_style = RenderStyle::new()
        .named_foreground(RenderNamedColor::BrightWhite)
        .named_background(RenderNamedColor::Blue)
        .bold();
    let inactive_style = RenderStyle::new()
        .named_foreground(RenderNamedColor::White)
        .named_background(RenderNamedColor::BrightBlack);
    let mut ops = vec![RenderOp::fill_rect(
        ExtensionRect::new(0, 0, u16::MAX, state.settings.height),
        ' ',
        inactive_style,
    )];
    let mut regions = Vec::with_capacity(state.snapshot.windows.len());
    let mut x = 0_u16;
    let visible = state
        .snapshot
        .windows
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(state.visible_limit())
        .collect::<Vec<_>>();
    let hidden_before = state.scroll_offset > 0;
    let hidden_after =
        state.scroll_offset.saturating_add(visible.len()) < state.snapshot.windows.len();
    if hidden_before {
        ops.push(RenderOp::text_run(x, 0, " ‹ ", inactive_style));
        x = x.saturating_add(3);
    }
    for (index, window) in visible {
        let label = if state.editing_window_id == Some(window.id) {
            format!(
                " [{}] ",
                truncate_to_width(
                    &state.edit_buffer,
                    usize::from(state.settings.maximum_label_width)
                )
            )
        } else {
            format!(" {} ", tab_label(&state.settings, window, index))
        };
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(label.as_str()))
            .unwrap_or(u16::MAX);
        let fact = state
            .settings
            .show_compact_facts
            .then(|| compact_fact(window))
            .flatten();
        let style = if let Some(fact) = fact {
            compact_fact_style(
                fact.role,
                if window.active {
                    active_style
                } else {
                    inactive_style
                },
            )
        } else if window.active {
            active_style
        } else if state.hovered_window_id == Some(window.id) {
            inactive_style
                .named_foreground(RenderNamedColor::BrightWhite)
                .named_background(RenderNamedColor::Blue)
        } else {
            inactive_style
        };
        ops.push(RenderOp::text_run(x, 0, label, style));
        regions.push(
            PluginSurfaceRegion::new(
                format!("window:{}", window.id),
                ExtensionRect::new(x, 0, width, state.settings.height),
            )
            .endpoint(bmux_plugin::AttachInputEndpoint {
                capability: "bmux.tab_strip.input".to_string(),
                interface_id: "presentation-input".to_string(),
                operation: "handle-input".to_string(),
            })
            .focusable(bmux_plugin::surface::PluginSurfaceCursor::Pointer),
        );
        x = x.saturating_add(width);
    }
    if hidden_after {
        ops.push(RenderOp::text_run(x, 0, " › ", inactive_style));
    }
    if let Some(target) = state.menu_window_id {
        let menu_style = RenderStyle::new()
            .named_foreground(RenderNamedColor::BrightWhite)
            .named_background(RenderNamedColor::Black);
        let selected_style = menu_style.named_background(RenderNamedColor::Blue).bold();
        let menu_x = visible_window_start(state, target).unwrap_or(0);
        let mut menu_x = menu_x;
        for (index, label) in ["Switch", "Rename", "Close"].iter().enumerate() {
            let text = format!(" {label} ");
            ops.push(RenderOp::text_run(
                menu_x,
                0,
                &text,
                if index == state.menu_selected {
                    selected_style
                } else {
                    menu_style
                },
            ));
            menu_x = menu_x.saturating_add(
                u16::try_from(unicode_width::UnicodeWidthStr::width(text.as_str()))
                    .unwrap_or(u16::MAX),
            );
        }
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

fn update_hover(event: &AttachInputEvent) -> bool {
    let target = event
        .hook_id
        .strip_prefix("bmux.tab_strip:strip:window:")
        .and_then(|target| Uuid::parse_str(target).ok());
    let hovered = match event.phase.as_str() {
        "enter" | "move" => target,
        "leave" => None,
        _ => return false,
    };
    let Ok(mut guard) = state().lock() else {
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

fn update_scroll(event: &AttachInputEvent) -> bool {
    if event.phase != "wheel" || event.wheel_delta == 0 {
        return false;
    }
    let Ok(mut guard) = state().lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    let maximum = companion
        .snapshot
        .windows
        .len()
        .saturating_sub(companion.visible_limit());
    let next = if event.wheel_delta > 0 {
        companion.scroll_offset.saturating_sub(1)
    } else {
        companion.scroll_offset.saturating_add(1).min(maximum)
    };
    if next == companion.scroll_offset {
        return true;
    }
    companion.scroll_offset = next;
    companion.revision = companion.revision.saturating_add(1).max(1);
    publish_companion(companion).is_ok()
}

fn visible_window_start(state: &CompanionState, target: Uuid) -> Option<u16> {
    let mut x = if state.scroll_offset > 0 { 3 } else { 0 };
    for (index, window) in state
        .snapshot
        .windows
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(state.visible_limit())
    {
        if window.id == target {
            return Some(x);
        }
        let label = format!(" {} ", tab_label(&state.settings, window, index));
        x = x.saturating_add(
            u16::try_from(unicode_width::UnicodeWidthStr::width(label.as_str())).ok()?,
        );
    }
    None
}

fn visible_window_at_col(state: &CompanionState, col: u16) -> Option<Uuid> {
    let mut x = if state.scroll_offset > 0 { 3 } else { 0 };
    for (index, window) in state
        .snapshot
        .windows
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(state.visible_limit())
    {
        let label = format!(" {} ", tab_label(&state.settings, window, index));
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(label.as_str())).ok()?;
        if col >= x && col < x.saturating_add(width) {
            return Some(window.id);
        }
        x = x.saturating_add(width);
    }
    None
}

fn update_drag(
    context: &NativeServiceContext,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    let source = event
        .hook_id
        .strip_prefix("bmux.tab_strip:strip:window:")
        .and_then(|target| Uuid::parse_str(target).ok())?;
    let mut guard = state().lock().ok()?;
    let companion = guard.as_mut()?;
    match event.phase.as_str() {
        "down" if event.button.as_deref() == Some("left") => {
            companion.pointer_source = Some(source);
            companion.pointer_moved = false;
            Some(AttachInputResult {
                consumed: true,
                capture_pointer: true,
                ..AttachInputResult::default()
            })
        }
        "drag" if companion.pointer_source == Some(source) => {
            companion.pointer_moved = true;
            Some(AttachInputResult {
                consumed: true,
                capture_pointer: true,
                ..AttachInputResult::default()
            })
        }
        "up" if companion.pointer_source == Some(source) => {
            let moved = companion.pointer_moved;
            let target = event
                .col
                .and_then(|col| visible_window_at_col(companion, col));
            companion.pointer_source = None;
            companion.pointer_moved = false;
            drop(guard);
            let result = if moved {
                target.filter(|target| *target != source).map_or_else(
                    || Ok(Ok(None)),
                    |target| {
                        let mut client = ServiceCallerDispatchClient::new(context);
                        block_on_typed_dispatch(windows_commands::client::move_window(
                            &mut client,
                            source,
                            target,
                            windows_commands::WindowMovePlacement::Before,
                        ))
                        .map(|result| result.map(Some))
                    },
                )
            } else {
                let mut client = ServiceCallerDispatchClient::new(context);
                block_on_typed_dispatch(windows_commands::client::switch_window(
                    &mut client,
                    source.to_string(),
                ))
                .map(|result| result.map(Some))
            };
            Some(match result {
                Ok(Ok(_)) => AttachInputResult {
                    consumed: true,
                    release_capture: true,
                    dirty: true,
                    ..AttachInputResult::default()
                },
                Ok(Err(error)) => AttachInputResult {
                    consumed: true,
                    release_capture: true,
                    status_message: Some(format!("window action failed: {error:?}")),
                    ..AttachInputResult::default()
                },
                Err(error) => AttachInputResult {
                    consumed: true,
                    release_capture: true,
                    status_message: Some(format!("window action unavailable: {error}")),
                    ..AttachInputResult::default()
                },
            })
        }
        _ => None,
    }
}

fn republish_companion(companion: &mut CompanionState) -> bool {
    companion.revision = companion.revision.saturating_add(1).max(1);
    publish_companion(companion).is_ok()
}

fn update_editor(
    context: &NativeServiceContext,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    if event.event_kind != "key" || !matches!(event.phase.as_str(), "press" | "repeat") {
        return None;
    }
    let mut guard = state().lock().ok()?;
    let companion = guard.as_mut()?;
    let editing = companion.editing_window_id?;
    let key = event.key.as_deref()?;
    match key {
        "esc" => {
            companion.editing_window_id = None;
            companion.edit_buffer.clear();
        }
        "backspace" => {
            companion.edit_buffer.pop();
        }
        "enter" => {
            let name = companion.edit_buffer.trim().to_string();
            if name.is_empty() {
                return Some(AttachInputResult {
                    consumed: true,
                    status_message: Some("window name must not be empty".to_string()),
                    ..AttachInputResult::default()
                });
            }
            companion.editing_window_id = None;
            companion.edit_buffer.clear();
            drop(guard);
            let mut client = ServiceCallerDispatchClient::new(context);
            return Some(
                match block_on_typed_dispatch(windows_commands::client::rename_window_by_id(
                    &mut client,
                    editing,
                    name,
                )) {
                    Ok(Ok(_)) => AttachInputResult {
                        consumed: true,
                        dirty: true,
                        ..AttachInputResult::default()
                    },
                    Ok(Err(error)) => AttachInputResult {
                        consumed: true,
                        status_message: Some(format!("window rename failed: {error:?}")),
                        ..AttachInputResult::default()
                    },
                    Err(error) => AttachInputResult {
                        consumed: true,
                        status_message: Some(format!("window rename unavailable: {error}")),
                        ..AttachInputResult::default()
                    },
                },
            );
        }
        value if value.chars().count() == 1 && companion.edit_buffer.len() < 4_096 => {
            companion.edit_buffer.push_str(value);
        }
        _ => {
            return Some(AttachInputResult {
                consumed: true,
                ..AttachInputResult::default()
            });
        }
    }
    let dirty = republish_companion(companion);
    Some(AttachInputResult {
        consumed: true,
        dirty,
        ..AttachInputResult::default()
    })
}

fn begin_rename(event: &AttachInputEvent) -> bool {
    if event.event_kind != "pointer"
        || event.phase != "down"
        || event.button.as_deref() != Some("middle")
    {
        return false;
    }
    let Some(target) = event
        .hook_id
        .strip_prefix("bmux.tab_strip:strip:window:")
        .and_then(|target| Uuid::parse_str(target).ok())
    else {
        return false;
    };
    let Ok(mut guard) = state().lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    let Some(window) = companion
        .snapshot
        .windows
        .iter()
        .find(|window| window.id == target)
    else {
        return false;
    };
    companion.editing_window_id = Some(target);
    companion.edit_buffer.clone_from(&window.name);
    republish_companion(companion)
}

fn begin_menu(event: &AttachInputEvent) -> bool {
    if event.event_kind != "pointer"
        || event.phase != "down"
        || event.button.as_deref() != Some("right")
    {
        return false;
    }
    let Some(target) = event
        .hook_id
        .strip_prefix("bmux.tab_strip:strip:window:")
        .and_then(|target| Uuid::parse_str(target).ok())
    else {
        return false;
    };
    let Ok(mut guard) = state().lock() else {
        return false;
    };
    let Some(companion) = guard.as_mut() else {
        return false;
    };
    companion.menu_window_id = Some(target);
    companion.menu_selected = 0;
    republish_companion(companion)
}

fn update_menu(
    context: &NativeServiceContext,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    if event.event_kind != "key" || !matches!(event.phase.as_str(), "press" | "repeat") {
        return None;
    }
    let mut guard = state().lock().ok()?;
    let companion = guard.as_mut()?;
    let target = companion.menu_window_id?;
    match event.key.as_deref()? {
        "left" => companion.menu_selected = companion.menu_selected.saturating_sub(1),
        "right" | "tab" => companion.menu_selected = (companion.menu_selected + 1).min(2),
        "esc" => {
            companion.menu_window_id = None;
        }
        "enter" => {
            let action = companion.menu_selected;
            companion.menu_window_id = None;
            if action == 1 {
                let Some(window) = companion
                    .snapshot
                    .windows
                    .iter()
                    .find(|window| window.id == target)
                else {
                    return Some(AttachInputResult::default());
                };
                companion.editing_window_id = Some(target);
                companion.edit_buffer.clone_from(&window.name);
                let dirty = republish_companion(companion);
                return Some(AttachInputResult {
                    consumed: true,
                    dirty,
                    ..AttachInputResult::default()
                });
            }
            drop(guard);
            let mut client = ServiceCallerDispatchClient::new(context);
            let result = if action == 0 {
                block_on_typed_dispatch(windows_commands::client::switch_window(
                    &mut client,
                    target.to_string(),
                ))
            } else {
                block_on_typed_dispatch(windows_commands::client::kill_window(
                    &mut client,
                    target.to_string(),
                    false,
                ))
            };
            return Some(match result {
                Ok(Ok(_)) => AttachInputResult {
                    consumed: true,
                    dirty: true,
                    ..AttachInputResult::default()
                },
                Ok(Err(error)) => AttachInputResult {
                    consumed: true,
                    status_message: Some(format!("window menu action failed: {error:?}")),
                    ..AttachInputResult::default()
                },
                Err(error) => AttachInputResult {
                    consumed: true,
                    status_message: Some(format!("window menu action unavailable: {error}")),
                    ..AttachInputResult::default()
                },
            });
        }
        _ => {
            return Some(AttachInputResult {
                consumed: true,
                ..AttachInputResult::default()
            });
        }
    }
    let dirty = republish_companion(companion);
    Some(AttachInputResult {
        consumed: true,
        dirty,
        ..AttachInputResult::default()
    })
}

fn handle_input(context: &NativeServiceContext, event: &AttachInputEvent) -> AttachInputResult {
    if let Some(result) = update_menu(context, event) {
        return result;
    }
    if let Some(result) = update_editor(context, event) {
        return result;
    }
    if begin_menu(event) {
        return AttachInputResult {
            consumed: true,
            capture_keyboard: vec![
                "left".to_string(),
                "right".to_string(),
                "tab".to_string(),
                "enter".to_string(),
                "esc".to_string(),
            ],
            dirty: true,
            ..AttachInputResult::default()
        };
    }
    if begin_rename(event) {
        return AttachInputResult {
            consumed: true,
            capture_keyboard: vec!["enter".to_string(), "esc".to_string()],
            dirty: true,
            ..AttachInputResult::default()
        };
    }
    if event.event_kind != "pointer" {
        return AttachInputResult::default();
    }
    if let Some(result) = update_drag(context, event) {
        return result;
    }
    if update_scroll(event) {
        return AttachInputResult {
            consumed: true,
            dirty: true,
            ..AttachInputResult::default()
        };
    }
    if update_hover(event) {
        return AttachInputResult {
            consumed: true,
            dirty: true,
            ..AttachInputResult::default()
        };
    }
    AttachInputResult::default()
}

bmux_plugin_sdk::export_plugin!(TabStripPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn republish_advances_past_retained_owner_revision() {
        uninstall();
        install(None).expect("install tab strip");
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
        uninstall();
    }

    #[test]
    fn layout_request_places_strip_on_configured_edge() {
        let bottom = layout_request(&Settings::default());
        let top = layout_request(&Settings {
            placement: Placement::Top,
            ..Settings::default()
        });
        assert!(matches!(
            bottom.operation,
            bmux_plugin::layout::LayoutOperation::Split {
                edge: LayoutEdge::Bottom,
                ..
            }
        ));
        assert!(matches!(
            top.operation,
            bmux_plugin::layout::LayoutOperation::Split {
                edge: LayoutEdge::Top,
                ..
            }
        ));
    }

    #[test]
    fn settings_validate_legacy_defaults_aliases_and_nested_values() {
        let value: toml::Value = toml::from_str(
            r##"
placement = "bottom"
height = 2
show_index = true
maximum_visible_tabs = 8
show_mode = false
hint_policy = "always"

[layout]
density = "compact"
overflow_style = "count"
align_active = "focus_bias"

[style]
separator_set = "ascii"
force_ascii = true

[colors]
bar_bg = "#112233"
"##,
        )
        .unwrap();
        let settings = Settings::parse(Some(&value)).unwrap();
        assert_eq!(settings.placement, Placement::Bottom);
        assert_eq!(settings.height, 2);
        assert_eq!(settings.label_template, "{index}:{name}");
        assert_eq!(settings.maximum_visible_tabs, Some(8));
        assert_eq!(settings.maximum_label_width, 20);
        assert!(!settings.show_mode);
        assert_eq!(settings.hint_policy, HintPolicy::Always);
        assert_eq!(settings.density, Density::Compact);
        assert_eq!(settings.overflow_style, OverflowStyle::Count);
        assert_eq!(settings.align_active, ActiveAlignment::FocusBias);
        assert_eq!(settings.separator_set, SeparatorSet::Ascii);
        assert!(settings.force_ascii);
        assert_eq!(settings.colors.bar_bg.as_deref(), Some("#112233"));

        let invalid: toml::Value = toml::from_str("height = 0").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
        let invalid_color: toml::Value =
            toml::from_str("[colors]\nbar_bg = 'not-a-color'").unwrap();
        assert!(Settings::parse(Some(&invalid_color)).is_err());
    }

    #[test]
    fn tab_labels_expand_templates_and_truncate_by_display_width() {
        let window = windows_list::WindowListEntry {
            id: Uuid::from_u128(8),
            name: "界界界".to_string(),
            active: true,
            workspace: "default".to_string(),
            workspace_id: uuid::Uuid::nil(),
        };
        let settings = Settings {
            label_template: "{index}:{name}".to_string(),
            maximum_label_width: 5,
            ..Settings::default()
        };
        assert_eq!(tab_label(&settings, &window, 0), "1:界");
    }

    #[test]
    fn local_presentation_updates_advance_retained_projection() {
        let mut state = CompanionState::new(Settings::default());
        let initial_revision = state.revision;
        state.replace_local_presentation(AttachLocalPresentationSnapshot {
            revision: 1,
            mode_id: "scroll".to_string(),
            mode_label: "SCROLL".to_string(),
            role_label: "read-only".to_string(),
            follow_label: Some("following abcdef12".to_string()),
            mode_modifier: Some("FROZEN".to_string()),
            hint: "scroll hint".to_string(),
            session_label: Some("session".to_string()),
            session_count: 2,
            context_label: Some("context".to_string()),
        });
        assert!(state.revision > initial_revision);
        assert_eq!(state.local_presentation.mode_label, "SCROLL");
        assert_eq!(state.local_presentation.role_label, "read-only");
    }

    #[test]
    fn hover_style_changes_only_the_target_window_operation() {
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
        assert_eq!(before.ops[1], after.ops[1]);
        assert_ne!(before.ops[2], after.ops[2]);
    }

    #[test]
    fn overflow_window_keeps_active_tab_visible_and_bounded() {
        let settings = Settings {
            maximum_visible_tabs: Some(2),
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
        assert_eq!(state.scroll_offset, 3);
        let surface = build_surface(&state, 1);
        assert_eq!(surface.interactive_regions.len(), 2);
        assert_eq!(
            surface.interactive_regions[1].local_id,
            format!("window:{}", Uuid::from_u128(4))
        );
        assert!(
            surface
                .ops
                .iter()
                .any(|op| matches!(op, RenderOp::TextRun { text, .. } if text == " ‹ "))
        );
    }

    #[test]
    fn visible_window_lookup_accounts_for_overflow_marker() {
        let settings = Settings {
            maximum_visible_tabs: Some(2),
            ..Settings::default()
        };
        let mut state = CompanionState::new(settings);
        state.scroll_offset = 1;
        state.snapshot.windows = (0..4)
            .map(|index| windows_list::WindowListEntry {
                id: Uuid::from_u128(index),
                name: format!("w{index}"),
                active: false,
                workspace: "default".to_string(),
                workspace_id: uuid::Uuid::nil(),
            })
            .collect();
        assert_eq!(visible_window_at_col(&state, 4), Some(Uuid::from_u128(1)));
        assert_eq!(visible_window_at_col(&state, 10), Some(Uuid::from_u128(2)));
    }

    #[test]
    fn tab_label_truncation_preserves_combining_graphemes() {
        assert_eq!(truncate_to_width("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn compact_fact_style_maps_warning_role() {
        assert_eq!(
            compact_fact_style(PresentationFactRole::Warning, RenderStyle::new()).fg,
            Some(bmux_plugin::RenderColor::Named(
                RenderNamedColor::BrightYellow
            ))
        );
    }

    #[test]
    fn surface_uses_stable_window_regions() {
        let mut state = CompanionState::new(Settings::default());
        let id = Uuid::from_u128(7);
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
