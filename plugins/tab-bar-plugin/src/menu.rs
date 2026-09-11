//! Client-local tab context menu. Only completed actions cross the service boundary.

use super::{
    BarStyles, CompanionHandle, CompanionState, LAYOUT_ID, OWNER, Placement, command_invocation,
    input_endpoint, republish_companion, tabs_commands,
};
use bmux_plugin::layout::{PluginLayoutId, resolve_plugin_layout};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceTarget,
};
use bmux_plugin::{AttachInputEvent, AttachInputResult, ExtensionRect, RenderOp};
use bmux_plugin_sdk::TypedServiceEndpoint;
use uuid::Uuid;

const LABELS: [&str; 3] = ["Switch", "Rename", "Close"];

fn popup_rect(companion: &CompanionState) -> Option<ExtensionRect> {
    let viewport = ExtensionRect::new(
        0,
        0,
        companion.local_presentation.viewport_cols,
        companion.local_presentation.viewport_rows,
    );
    if viewport.w == 0 || viewport.h == 0 {
        return None;
    }
    let mut requests = companion.layouts.requests();
    // Use this companion's layout intent even before its first publication.
    requests.retain(|request| request.id.owner_plugin_id != OWNER);
    requests.push(super::layout_request(&companion.settings));
    let layout = resolve_plugin_layout(viewport, (1, 1), &requests).ok()?;
    let strip = layout
        .allocations
        .iter()
        .find(|allocation| allocation.id == PluginLayoutId::new(OWNER, LAYOUT_ID))?
        .rect;
    let tab = super::build_surface(companion, companion.revision)
        .interactive_regions
        .into_iter()
        .find(|region| {
            Some(region.local_id.as_str())
                == companion
                    .menu_tab_id
                    .map(|id| format!("tab:{id}"))
                    .as_deref()
        })?;
    let width = 12.min(viewport.w);
    let height = 5.min(viewport.h);
    let x = strip.x.saturating_add(tab.rect.x).min(viewport.w - width);
    let y = match companion.settings.placement {
        Placement::Top => strip.y.saturating_add(strip.h).min(viewport.h - height),
        Placement::Bottom => strip.y.saturating_sub(height),
    };
    Some(ExtensionRect::new(x, y, width, height))
}

pub fn surfaces(companion: &CompanionState, revision: u64) -> Vec<PluginSurface> {
    if companion.menu_tab_id.is_none() {
        return Vec::new();
    }
    let Some(rect) = popup_rect(companion) else {
        return Vec::new();
    };
    let viewport = ExtensionRect::new(
        0,
        0,
        companion.local_presentation.viewport_cols,
        companion.local_presentation.viewport_rows,
    );
    let styles = BarStyles::resolve(&companion.settings, &companion.local_presentation);
    let mut backdrop = PluginSurface::layout(
        PluginSurfaceId::new(
            OWNER,
            "menu-dismiss",
            Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0002),
        ),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        Vec::new(),
    )
    .order(100, 0)
    .modal(true)
    .interactive_region(PluginSurfaceRegion::new("dismiss", viewport).endpoint(input_endpoint()));
    backdrop.target = PluginSurfaceTarget::Explicit(viewport);
    let mut ops = vec![RenderOp::fill_rect(
        ExtensionRect::new(0, 0, rect.w, rect.h),
        ' ',
        styles.base,
    )];
    let mut popup = PluginSurface::layout(
        PluginSurfaceId::new(
            OWNER,
            "menu",
            Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0003),
        ),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        Vec::new(),
    )
    .order(100, 1)
    .opaque(true)
    .modal(true);
    popup.target = PluginSurfaceTarget::Explicit(rect);
    // Keep item regions non-focusable: the originating tab remains the stable
    // keyboard target, including when Rename transfers into its inline editor.
    for (index, label) in LABELS.iter().enumerate() {
        let row = u16::try_from(index).unwrap_or(0) + u16::from(rect.h >= 5);
        if row >= rect.h {
            break;
        }
        let selected = companion.menu_selected == index;
        ops.push(RenderOp::text_run(
            0,
            row,
            format!(" {} {label}", if selected { "›" } else { " " }),
            if selected { styles.mode } else { styles.base },
        ));
        popup = popup.interactive_region(
            PluginSurfaceRegion::new(
                format!("item:{index}"),
                ExtensionRect::new(0, row, rect.w, 1),
            )
            .endpoint(input_endpoint()),
        );
    }
    popup.ops = ops;
    vec![backdrop, popup]
}

#[allow(clippy::significant_drop_tightening)] // Publish the surface under the same lock as its interaction state.
pub fn handle_input(
    owner: &CompanionHandle,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    let mut guard = owner.lock().ok()?;
    let companion = guard.as_mut()?;
    let mut result = transition(companion, event)?;
    if result.dirty {
        result.dirty = republish_companion(companion);
    }
    Some(result)
}

fn transition(
    companion: &mut CompanionState,
    event: &AttachInputEvent,
) -> Option<AttachInputResult> {
    let pointer = event.event_kind == "pointer";
    let down = pointer && event.phase == "down";
    if companion.menu_tab_id.is_none() {
        if !down || event.button.as_deref() != Some("right") {
            return None;
        }
        let target = event
            .hook_id
            .strip_prefix("bmux.tab_bar:strip:tab:")
            .and_then(|id| Uuid::parse_str(id).ok())?;
        if !companion.snapshot.tabs.iter().any(|tab| tab.id == target) {
            return None;
        }
        companion.menu_tab_id = Some(target);
        // Do not capture input unless there is a visible popup to interact with.
        if popup_rect(companion).is_none() {
            companion.menu_tab_id = None;
            return Some(AttachInputResult::default());
        }
        companion.menu_selected = 0;
        companion.editing_tab_id = None;
        companion.pointer_source = None;
        companion.drag_target = None;
        return Some(AttachInputResult {
            consumed: true,
            dirty: true,
            capture_keyboard: vec!["*".to_string()],
            ..AttachInputResult::default()
        });
    }
    let mut result = AttachInputResult {
        consumed: true,
        ..AttachInputResult::default()
    };
    let mut activate = false;
    if pointer {
        let item = event
            .hook_id
            .strip_prefix("bmux.tab_bar:menu:item:")
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| *index < LABELS.len());
        if let Some(item) = item {
            if matches!(event.phase.as_str(), "enter" | "move" | "down") {
                companion.menu_selected = item;
                result.dirty = true;
            }
            activate = down && event.button.as_deref() == Some("left");
        } else if down {
            companion.menu_tab_id = None;
            result.release_capture = true;
            result.dirty = true;
        }
    } else if event.event_kind == "key" && matches!(event.phase.as_str(), "press" | "repeat") {
        match event.key.as_deref()? {
            "up" | "left" => companion.menu_selected = companion.menu_selected.saturating_sub(1),
            "down" | "right" | "tab" => {
                companion.menu_selected = (companion.menu_selected + 1).min(2);
            }
            "esc" => {
                companion.menu_tab_id = None;
                result.release_capture = true;
            }
            "enter" => activate = true,
            _ => return Some(result),
        }
        result.dirty = true;
    }
    if activate {
        activate_selection(companion, &mut result);
    }
    Some(result)
}

fn activate_selection(companion: &mut CompanionState, result: &mut AttachInputResult) {
    let Some(target) = companion.menu_tab_id.take() else {
        return;
    };
    result.dirty = true;
    result.release_capture = true;
    result.service_invocation = match companion.menu_selected {
        0 => command_invocation(
            bmux_plugin::AttachInputEndpoint {
                capability: tabs_commands::client::SwitchTabEndpoint::CAPABILITY.to_string(),
                interface_id: tabs_commands::client::SwitchTabEndpoint::INTERFACE_ID.to_string(),
                operation: tabs_commands::client::SwitchTabEndpoint::OPERATION.to_string(),
            },
            &tabs_commands::client::SwitchTabRequest {
                target: target.to_string(),
            },
        ),
        1 => {
            if let Some(tab) = companion.snapshot.tabs.iter().find(|tab| tab.id == target) {
                companion.editing_tab_id = Some(target);
                companion.edit_buffer =
                    bmux_text_edit::TextEditBuffer::from_text(tab.name.clone()).into();
                companion.edit_buffer.select_all();
                result.release_capture = false;
                result.capture_keyboard = vec!["*".to_string()];
            }
            None
        }
        _ => command_invocation(
            bmux_plugin::AttachInputEndpoint {
                capability: tabs_commands::client::KillTabEndpoint::CAPABILITY.to_string(),
                interface_id: tabs_commands::client::KillTabEndpoint::INTERFACE_ID.to_string(),
                operation: tabs_commands::client::KillTabEndpoint::OPERATION.to_string(),
            },
            &tabs_commands::client::KillTabRequest {
                target: target.to_string(),
                force_local: false,
            },
        ),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Settings, companion_surfaces, tabs_list};

    fn companion() -> CompanionState {
        let mut companion = CompanionState::new(Settings::default());
        companion.local_presentation.viewport_cols = 80;
        companion.local_presentation.viewport_rows = 24;
        companion.replace_tabs(tabs_list::TabListSnapshot {
            revision: 1,
            tabs: vec![tabs_list::TabListEntry {
                id: Uuid::from_u128(7),
                name: "test".to_string(),
                active: true,
                workspace: "default".to_string(),
                workspace_id: Uuid::nil(),
            }],
        });
        companion
    }

    fn event(
        kind: &str,
        phase: &str,
        key: Option<&str>,
        button: Option<&str>,
        hook: String,
    ) -> AttachInputEvent {
        AttachInputEvent {
            event_kind: kind.to_string(),
            phase: phase.to_string(),
            key: key.map(str::to_string),
            button: button.map(str::to_string),
            hook_id: hook,
            col: Some(3),
            row: Some(0),
            wheel_delta: 0,
            modifiers: bmux_plugin::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
        }
    }

    fn open(companion: &mut CompanionState) -> AttachInputResult {
        transition(
            companion,
            &event(
                "pointer",
                "down",
                None,
                Some("right"),
                format!("bmux.tab_bar:strip:tab:{}", Uuid::from_u128(7)),
            ),
        )
        .unwrap()
    }

    fn key(companion: &mut CompanionState, key: &str) -> Option<AttachInputResult> {
        transition(
            companion,
            &event("key", "press", Some(key), None, String::new()),
        )
    }

    #[test]
    fn popup_geometry_uses_the_companions_layout_registry() {
        let mut first = companion();
        first.layouts = std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4));
        first.menu_tab_id = Some(Uuid::from_u128(7));
        let mut second = first.clone();
        second.layouts = std::sync::Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(4));
        let baseline = popup_rect(&second).unwrap();
        first
            .layouts
            .publish(
                "sidebar",
                bmux_plugin::layout::PluginLayoutSnapshot {
                    revision: 1,
                    requests: vec![bmux_plugin::layout::PluginLayoutRequest::split(
                        PluginLayoutId::new("sidebar", "left"),
                        i32::MIN,
                        bmux_plugin::layout::LayoutEdge::Left,
                        bmux_plugin::layout::LayoutExtent::Cells(20),
                    )],
                },
            )
            .unwrap();
        assert_eq!(popup_rect(&first).unwrap().x, baseline.x + 20);
        assert_eq!(popup_rect(&second).unwrap(), baseline);
    }

    #[test]
    fn opening_retains_tab_focus_and_publishes_popup_then_escape_releases_input() {
        let mut companion = companion();
        let before = crate::build_surface(&companion, 1).interactive_regions;
        let opened = open(&mut companion);
        assert!(opened.consumed && opened.dirty);
        assert!(opened.service_invocation.is_none());
        let surfaces = companion_surfaces(&companion, 2);
        assert_eq!(surfaces.len(), 3);
        assert_eq!(surfaces[0].interactive_regions, before);
        assert!(
            surfaces[2]
                .ops
                .iter()
                .any(|op| matches!(op, RenderOp::TextRun { text, .. } if text.contains("Rename")))
        );
        key(&mut companion, "down").unwrap();
        assert_eq!(companion.menu_selected, 1);
        assert_eq!(
            companion_surfaces(&companion, 3)[0].interactive_regions,
            before
        );
        assert!(key(&mut companion, "esc").unwrap().release_capture);
        assert_eq!(companion_surfaces(&companion, 4).len(), 1);
        assert!(key(&mut companion, "ctrl-a").is_none());
        assert!(key(&mut companion, "x").is_none());
    }

    #[test]
    fn actions_use_generated_requests_and_rename_keeps_keyboard_focus() {
        for selection in [0, 1, 2] {
            let mut companion = companion();
            open(&mut companion);
            companion.menu_selected = selection;
            let result = key(&mut companion, "enter").unwrap();
            assert!(companion.menu_tab_id.is_none());
            if selection == 1 {
                assert!(!result.release_capture);
                assert!(result.service_invocation.is_none());
                assert_eq!(companion.editing_tab_id, Some(Uuid::from_u128(7)));
                assert_eq!(companion.edit_buffer.text(), "test");
            } else {
                assert!(result.release_capture);
                let invocation = result.service_invocation.unwrap();
                if selection == 0 {
                    let request: tabs_commands::client::SwitchTabRequest =
                        bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
                    assert_eq!(request.target, Uuid::from_u128(7).to_string());
                } else {
                    let request: tabs_commands::client::KillTabRequest =
                        bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
                    assert_eq!(request.target, Uuid::from_u128(7).to_string());
                    assert!(!request.force_local);
                }
            }
        }
    }

    #[test]
    fn pointer_activation_and_outside_dismissal_are_local() {
        let mut companion = companion();
        open(&mut companion);
        let result = transition(
            &mut companion,
            &event(
                "pointer",
                "down",
                None,
                Some("left"),
                "bmux.tab_bar:menu:item:2".to_string(),
            ),
        )
        .unwrap();
        assert!(result.release_capture && result.service_invocation.is_some());
        open(&mut companion);
        let result = transition(
            &mut companion,
            &event(
                "pointer",
                "down",
                None,
                Some("left"),
                "bmux.tab_bar:menu-dismiss:dismiss".to_string(),
            ),
        )
        .unwrap();
        assert!(result.release_capture && result.service_invocation.is_none());
        assert!(companion.menu_tab_id.is_none());
    }

    #[test]
    fn geometry_is_bounded_and_missing_viewport_never_captures() {
        for placement in [Placement::Top, Placement::Bottom] {
            for (cols, rows) in [(80, 24), (4, 2), (1, 1)] {
                let mut companion = companion();
                companion.settings.placement = placement;
                companion.local_presentation.viewport_cols = cols;
                companion.local_presentation.viewport_rows = rows;
                let opened = open(&mut companion);
                if let Some(rect) = popup_rect(&companion) {
                    assert!(rect.x + rect.w <= cols && rect.y + rect.h <= rows);
                } else {
                    // Extremely narrow strips have no hittable tab to anchor to.
                    assert!(!opened.consumed);
                    assert!(companion.menu_tab_id.is_none());
                }
            }
        }
        let mut companion = companion();
        companion.local_presentation.viewport_rows = 0;
        assert!(!open(&mut companion).consumed);
        assert!(companion.menu_tab_id.is_none());
    }

    #[test]
    fn removing_target_dismisses_and_companions_do_not_share_state() {
        let mut first = companion();
        let second = companion();
        open(&mut first);
        assert!(second.menu_tab_id.is_none());
        first.replace_tabs(tabs_list::TabListSnapshot {
            revision: 2,
            tabs: Vec::new(),
        });
        assert!(first.menu_tab_id.is_none());
        assert!(key(&mut first, "x").is_none());
    }
}
