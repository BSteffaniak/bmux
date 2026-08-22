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
    AttachInputEvent, AttachInputResult, ExtensionRect, RenderNamedColor, RenderOp, RenderStyle,
    ServiceCallerDispatchClient, block_on_typed_dispatch,
};
use bmux_plugin_sdk::prelude::*;
use bmux_windows_plugin_api::{windows_commands, windows_list};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const OWNER: &str = "bmux.tab_strip";
const LAYOUT_ID: &str = "strip";
const SURFACE_ID: &str = "strip";
const RETAINED_ID: Uuid = Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0001);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    placement: Placement,
    height: u16,
    order: i32,
    show_index: bool,
    label_template: String,
    maximum_label_width: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            placement: Placement::Top,
            height: 1,
            order: 100,
            show_index: true,
            label_template: "{index}{name}".to_string(),
            maximum_label_width: 32,
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
                "top" => Placement::Top,
                "bottom" => Placement::Bottom,
                other => {
                    return Err(PluginCommandError::invalid_arguments(format!(
                        "bmux.tab_strip placement must be 'top' or 'bottom', got {other:?}"
                    )));
                }
            };
        }
        if let Some(height) = table.get("height").and_then(toml::Value::as_integer) {
            settings.height = u16::try_from(height)
                .ok()
                .filter(|height| (1..=4).contains(height))
                .ok_or_else(|| {
                    PluginCommandError::invalid_arguments(
                        "bmux.tab_strip height must be between 1 and 4",
                    )
                })?;
        }
        if let Some(order) = table.get("order").and_then(toml::Value::as_integer) {
            settings.order = i32::try_from(order).map_err(|_| {
                PluginCommandError::invalid_arguments("bmux.tab_strip order must fit in an i32")
            })?;
        }
        if let Some(show_index) = table.get("show_index").and_then(toml::Value::as_bool) {
            settings.show_index = show_index;
        }
        if let Some(template) = table.get("label_template").and_then(toml::Value::as_str) {
            if template.len() > 4_096 {
                return Err(PluginCommandError::invalid_arguments(
                    "bmux.tab_strip label_template must not exceed 4096 bytes",
                ));
            }
            settings.label_template = template.to_string();
        }
        if let Some(width) = table
            .get("maximum_label_width")
            .and_then(toml::Value::as_integer)
        {
            settings.maximum_label_width = u16::try_from(width)
                .ok()
                .filter(|width| *width > 0)
                .ok_or_else(|| {
                    PluginCommandError::invalid_arguments(
                        "bmux.tab_strip maximum_label_width must be positive",
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
    publish(initial.as_ref().clone())?;
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
    drop(guard);
    global_plugin_surface_registry()
        .publish(
            OWNER,
            PluginSurfaceSnapshot {
                revision,
                surfaces: vec![surface],
            },
        )
        .map_err(|error| format!("publishing tab-strip surface: {error:?}"))?;
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

fn tab_label(settings: &Settings, window: &windows_list::WindowListEntry, index: usize) -> String {
    const INDEX_TOKEN: &str = concat!("{", "index}");
    let index = if settings.show_index {
        format!("{}:", index.saturating_add(1))
    } else {
        String::new()
    };
    let label = settings
        .label_template
        .replace("{{", "\u{0}")
        .replace("}}", "\u{1}")
        .replace(INDEX_TOKEN, &index)
        .replace("{name}", &window.name)
        .replace("{id}", &window.id.to_string())
        .replace("{active}", if window.active { "active" } else { "idle" })
        .replace('\u{0}', "{")
        .replace('\u{1}', "}");
    truncate_to_width(&label, usize::from(settings.maximum_label_width))
}

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
    for (index, window) in state.snapshot.windows.iter().enumerate() {
        let label = format!(" {} ", tab_label(&state.settings, window, index));
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(label.as_str()))
            .unwrap_or(u16::MAX);
        ops.push(RenderOp::text_run(
            x,
            0,
            label,
            if window.active {
                active_style
            } else {
                inactive_style
            },
        ));
        regions.push(
            PluginSurfaceRegion::new(
                format!("window:{}", window.id),
                ExtensionRect::new(x, 0, width, state.settings.height),
            )
            .endpoint(bmux_plugin::AttachInputEndpoint {
                capability: "bmux.tab_strip.input".to_string(),
                interface_id: "presentation-input".to_string(),
                operation: "handle-input".to_string(),
            }),
        );
        x = x.saturating_add(width);
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
    let Some(target) = event.hook_id.strip_prefix("bmux.tab_strip:strip:window:") else {
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

bmux_plugin_sdk::export_plugin!(TabStripPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_validate_placement_and_height() {
        let settings: toml::Value =
            toml::from_str("placement = 'bottom'\nheight = 2\nshow_index = false").unwrap();
        assert_eq!(
            Settings::parse(Some(&settings)).unwrap(),
            Settings {
                placement: Placement::Bottom,
                height: 2,
                order: 100,
                show_index: false,
                label_template: "{index}{name}".to_string(),
                maximum_label_width: 32,
            }
        );
        let invalid: toml::Value = toml::from_str("height = 0").unwrap();
        assert!(Settings::parse(Some(&invalid)).is_err());
    }

    #[test]
    fn tab_labels_expand_templates_and_truncate_by_display_width() {
        let window = windows_list::WindowListEntry {
            id: Uuid::from_u128(8),
            name: "界界界".to_string(),
            active: true,
        };
        let settings = Settings {
            maximum_label_width: 5,
            ..Settings::default()
        };
        assert_eq!(tab_label(&settings, &window, 0), "1:界");
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
