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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    placement: Placement,
    width: u16,
    minimum_width: u16,
    maximum_width: u16,
    order: i32,
    show_index: bool,
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
                requests: vec![layout_request(settings)],
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

fn layout_request(settings: Settings) -> PluginLayoutRequest {
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
        RenderOp::text_run(2, 0, " Windows ", active),
    ];
    let mut regions = Vec::with_capacity(state.snapshot.windows.len());
    let content_width = usize::from(width.saturating_sub(4));
    for (index, window) in state.snapshot.windows.iter().enumerate() {
        let Ok(row) = u16::try_from(index.saturating_add(1)) else {
            break;
        };
        let prefix = if state.settings.show_index {
            format!("{} ", index.saturating_add(1))
        } else {
            String::new()
        };
        let marker = if window.active { "●" } else { "○" };
        let text = truncate_to_width(&format!("{marker} {prefix}{}", window.name), content_width);
        ops.push(RenderOp::text_run(
            2,
            row,
            text,
            if window.active { active } else { inactive },
        ));
        regions.push(
            PluginSurfaceRegion::new(
                format!("window:{}", window.id),
                ExtensionRect::new(1, row, width.saturating_sub(2), 1),
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
        assert_eq!(
            Settings::parse(Some(&settings)).unwrap().placement,
            Placement::Right
        );
        let invalid: toml::Value =
            toml::from_str("width = 10\nminimum_width = 12\nmaximum_width = 40").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn truncation_is_unicode_cell_safe() {
        assert_eq!(truncate_to_width("ab界cd", 4), "ab界");
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
