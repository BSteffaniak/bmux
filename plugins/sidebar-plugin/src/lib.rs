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
        Ok(settings)
    }
}

#[derive(Debug, Clone)]
struct CompanionState {
    settings: Settings,
    revision: u64,
    snapshot: windows_list::WindowListSnapshot,
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
        }
    }

    fn replace_windows(&mut self, snapshot: windows_list::WindowListSnapshot) {
        if self.snapshot != snapshot {
            self.snapshot = snapshot;
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
        LayoutExtent::Bounded {
            preferred: settings.width,
            minimum: settings.minimum_width,
            maximum: settings.maximum_width,
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
    let mut result = String::new();
    let mut width = 0_usize;
    for ch in value.chars() {
        let cell_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width.saturating_add(cell_width) > maximum {
            break;
        }
        result.push(ch);
        width = width.saturating_add(cell_width);
    }
    result
}

fn render_template(
    template: &str,
    window: &windows_list::WindowListEntry,
    index: usize,
    show_index: bool,
) -> String {
    const MARKER_TOKEN: &str = concat!("{", "marker}");
    const INDEX_TOKEN: &str = concat!("{", "index}");
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
    let mut ops = vec![
        RenderOp::fill_rect(ExtensionRect::new(0, 0, width, u16::MAX), ' ', background),
        RenderOp::border(
            ExtensionRect::new(0, 0, width, u16::MAX),
            BorderGlyphs::square(),
            background,
        ),
        RenderOp::text_run(2, 0, format!(" {} ", state.settings.heading), active),
    ];
    let mut regions = Vec::with_capacity(state.snapshot.windows.len());
    let content_width = usize::from(width.saturating_sub(4));
    let mut row = 1_u16;
    for (index, window) in state.snapshot.windows.iter().enumerate() {
        let start_row = row;
        let title = render_template(
            &state.settings.title_template,
            window,
            index,
            state.settings.show_index,
        );
        let title = truncate_to_width(&title, content_width);
        ops.push(RenderOp::text_run(
            2,
            row,
            title,
            if window.active { active } else { inactive },
        ));
        row = row.saturating_add(1);
        let description = render_template(
            &state.settings.description_template,
            window,
            index,
            state.settings.show_index,
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

fn handle_input(context: &NativeServiceContext, event: &AttachInputEvent) -> AttachInputResult {
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
    fn settings_validate_width_bounds() {
        let settings: toml::Value = toml::from_str(
            "placement = 'right'\nwidth = 24\nminimum_width = 12\nmaximum_width = 40",
        )
        .unwrap();
        let settings = Settings::parse(Some(&settings)).unwrap();
        assert_eq!(settings.placement, Placement::Right);
        assert_eq!(settings.title_template, "{marker} {index}{name}");
        let invalid: toml::Value =
            toml::from_str("width = 10\nminimum_width = 12\nmaximum_width = 40").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
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
            render_template("{{literal}} {index}{name} {active}", &window, 1, true),
            "{literal} 2 build active"
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
