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
struct CompanionState {
    settings: Settings,
    revision: u64,
    snapshot: windows_list::WindowListSnapshot,
    hovered_window_id: Option<Uuid>,
    scroll_offset: usize,
}

impl CompanionState {
    const fn new(settings: Settings) -> Self {
        Self {
            settings,
            revision: 0,
            snapshot: windows_list::WindowListSnapshot {
                windows: Vec::new(),
                revision: 0,
            },
            hovered_window_id: None,
            scroll_offset: 0,
        }
    }

    fn replace_windows(&mut self, snapshot: windows_list::WindowListSnapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
            let maximum_offset = self
                .snapshot
                .windows
                .len()
                .saturating_sub(self.settings.maximum_visible_items);
            self.scroll_offset = self.scroll_offset.min(maximum_offset);
            if let Some(active) = self
                .snapshot
                .windows
                .iter()
                .position(|window| window.active)
            {
                if active < self.scroll_offset {
                    self.scroll_offset = active;
                } else if active
                    >= self
                        .scroll_offset
                        .saturating_add(self.settings.maximum_visible_items)
                {
                    self.scroll_offset = active
                        .saturating_add(1)
                        .saturating_sub(self.settings.maximum_visible_items);
                }
            }
            self.revision = self.revision.saturating_add(1).max(1);
        }
    }
}

fn state() -> &'static Mutex<Option<CompanionState>> {
    static STATE: OnceLock<Mutex<Option<CompanionState>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

#[derive(Default)]
pub struct SidebarPlugin;

impl RustPlugin for SidebarPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let settings = Settings::parse(context.settings.as_ref())?;
        *state()
            .lock()
            .map_err(|_| PluginCommandError::failed("sidebar state lock poisoned"))? =
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
        .map_err(|_| "sidebar state lock poisoned".to_string())?;
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
        .map_err(|error| format!("publishing sidebar layout: {error:?}"))?;
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
    publish(initial.as_ref().clone())?;
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("sidebar companion requires an async runtime: {error}"))?;
    handle.spawn(async move {
        while receiver.changed().await.is_ok() {
            let snapshot = receiver.borrow_and_update().as_ref().clone();
            if let Err(error) = publish(snapshot) {
                tracing::warn!(%error, "sidebar publication failed");
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

fn publish(snapshot: windows_list::WindowListSnapshot) -> Result<(), String> {
    let mut guard = state()
        .lock()
        .map_err(|_| "sidebar state lock poisoned".to_string())?;
    let Some(companion) = guard.as_mut() else {
        return Ok(());
    };
    companion.replace_windows(snapshot);
    let revision = companion.revision.max(1);
    let surface = build_surface(companion, revision);
    drop(guard);
    global_plugin_surface_registry()
        .publish(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface],
            },
        )
        .map_err(|error| format!("publishing sidebar surface: {error:?}"))?;
    Ok(())
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
    let mut rows = 0_u16;
    let mut remaining = text.trim();
    while !remaining.is_empty() && rows < maximum_rows {
        let mut split = remaining.len();
        let mut cells = 0_usize;
        let mut last_space = None;
        for (offset, ch) in remaining.char_indices() {
            let next =
                cells.saturating_add(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0));
            if next > width {
                split = last_space.unwrap_or(offset);
                break;
            }
            cells = next;
            if ch.is_whitespace() {
                last_space = Some(offset);
            }
        }
        let (line, tail) = remaining.split_at(split);
        let line = line.trim_end();
        if !line.is_empty() {
            ops.push(RenderOp::text_run(
                x,
                start_row.saturating_add(rows),
                line,
                style,
            ));
            rows = rows.saturating_add(1);
        }
        remaining = tail.trim_start();
        if split == 0 {
            break;
        }
    }
    rows
}

#[allow(clippy::too_many_lines)] // Scene construction is one ordered retained projection; splitting obscures row accounting.
fn build_surface(state: &CompanionState, revision: u64) -> PluginSurface {
    let width = state.settings.width;
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
    let height = if state.settings.content_height {
        u16::try_from(
            state
                .snapshot
                .windows
                .len()
                .min(state.settings.maximum_visible_items)
                .saturating_add(2),
        )
        .unwrap_or(u16::MAX)
    } else {
        u16::MAX
    };
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
    let content_width = usize::from(width.saturating_sub(4));
    let mut row = 1_u16;
    for (index, window) in state
        .snapshot
        .windows
        .iter()
        .enumerate()
        .skip(state.scroll_offset)
        .take(state.settings.maximum_visible_items)
    {
        let fact = window_fact(window);
        let start_row = row;
        let title = render_template(
            &state.settings.title_template,
            window,
            index,
            state.settings.show_index,
            fact.as_ref(),
        );
        let title = truncate_to_width(&title, content_width);
        let item_style = fact.as_ref().map_or_else(
            || {
                if window.active {
                    active
                } else if state.hovered_window_id == Some(window.id) {
                    inactive
                        .named_foreground(RenderNamedColor::BrightWhite)
                        .named_background(RenderNamedColor::Blue)
                } else {
                    inactive
                }
            },
            |fact| fact_style(fact.role, if window.active { active } else { inactive }),
        );
        ops.push(RenderOp::text_run(2, row, title, item_style));
        row = row.saturating_add(1);
        let description = render_template(
            &state.settings.description_template,
            window,
            index,
            state.settings.show_index,
            fact.as_ref(),
        );
        row = row.saturating_add(push_wrapped_text(
            &mut ops,
            &description,
            3,
            row,
            content_width.saturating_sub(1),
            2,
            inactive.dim(),
        ));
        let status = render_template(
            &state.settings.status_template,
            window,
            index,
            state.settings.show_index,
            fact.as_ref(),
        );
        if !status.is_empty() {
            ops.push(RenderOp::text_run(
                3,
                row,
                truncate_to_width(&status, content_width.saturating_sub(1)),
                if window.active {
                    active
                } else {
                    inactive.dim()
                },
            ));
            row = row.saturating_add(1);
        }
        regions.push(
            PluginSurfaceRegion::new(
                format!("window:{}", window.id),
                ExtensionRect::new(
                    1,
                    start_row,
                    width.saturating_sub(2),
                    row.saturating_sub(start_row).max(1),
                ),
            )
            .endpoint(bmux_plugin::AttachInputEndpoint {
                capability: "bmux.sidebar.input".to_string(),
                interface_id: "presentation-input".to_string(),
                operation: "handle-input".to_string(),
            }),
        );
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
        .strip_prefix("bmux.sidebar:sidebar:window:")
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
    let revision = companion.revision;
    let surface = build_surface(companion, revision);
    drop(guard);
    global_plugin_surface_registry()
        .publish(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface],
            },
        )
        .is_ok()
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
        .saturating_sub(companion.settings.maximum_visible_items);
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
    let revision = companion.revision;
    let surface = build_surface(companion, revision);
    drop(guard);
    global_plugin_surface_registry()
        .publish(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface],
            },
        )
        .is_ok()
}

fn handle_input(context: &NativeServiceContext, event: &AttachInputEvent) -> AttachInputResult {
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
    if event.event_kind != "pointer"
        || event.phase != "down"
        || event.button.as_deref() != Some("left")
    {
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
                },
                windows_list::WindowListEntry {
                    id: second,
                    name: "two".to_string(),
                    active: false,
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
                },
                windows_list::WindowListEntry {
                    id: second,
                    name: "two".to_string(),
                    active: false,
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
                })
                .collect(),
            revision: 2,
        });
        let surface = build_surface(&state, 2);
        assert_eq!(surface.interactive_regions.len(), 32);
        assert_eq!(state.scroll_offset, 1_968);
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
