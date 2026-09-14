//! Client-local tab context menu. Only completed actions cross the service boundary.

use super::{
    CompanionHandle, CompanionState, LAYOUT_ID, OWNER, Placement, command_invocation,
    input_endpoint, republish_companion, tabs_commands,
};
use bmux_plugin::component_viewport::{CommittedComponentViewport, ComponentViewport};
use bmux_plugin::layout::{PluginLayoutId, resolve_plugin_layout};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceTarget,
};
use bmux_plugin::{AttachInputEvent, AttachInputResult, ExtensionRect};
use bmux_plugin_sdk::TypedServiceEndpoint;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use std::cell::Cell;

use bmux_tui::prelude::Style;
use bmux_tui_components::menu::{MenuComponent, MenuItem, MenuOutcome, MenuPolicy, MenuState};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing, ModalTheme};
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
                    .map(|id| {
                        format!(
                            "{}:{id}",
                            if companion.menu.workspace {
                                "workspace"
                            } else {
                                "tab"
                            }
                        )
                    })
                    .as_deref()
        })?;
    let width = (if !companion.menu.group.is_empty() || companion.menu.confirming {
        48
    } else if companion.menu.workspace {
        22
    } else {
        12
    })
    .min(viewport.w);
    let height = u16::try_from(items(companion).len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(viewport.h);
    let x = strip.x.saturating_add(tab.rect.x).min(viewport.w - width);
    let y = match companion.settings.placement {
        Placement::Top => strip.y.saturating_add(strip.h).min(viewport.h - height),
        Placement::Bottom => strip.y.saturating_sub(height),
    };
    Some(ExtensionRect::new(x, y, width, height))
}

#[derive(Debug, Clone)]
pub struct MenuInput {
    pub group: Vec<Uuid>,
    pub destinations: Option<Vec<(Uuid, String)>>,
    pub workspace: bool,
    pub confirming: bool,
    pub state: MenuState,
    pub geometry: CommittedComponentViewport,
}
impl Default for MenuInput {
    fn default() -> Self {
        Self {
            group: Vec::new(),
            destinations: None,
            workspace: false,
            confirming: false,
            state: MenuState::new(Some(0)),
            geometry: CommittedComponentViewport::default(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct MoveSelectionRequest {
    pub action: String,
    pub tabs: Vec<Uuid>,
    pub workspace: Uuid,
}

fn items(companion: &CompanionState) -> Vec<MenuItem> {
    if let Some(destinations) = &companion.menu.destinations {
        return [
            MenuItem::new("cancel", "Cancel"),
            MenuItem::new("new", "New workspace"),
        ]
        .into_iter()
        .chain(
            destinations
                .iter()
                .map(|(id, name)| MenuItem::new(id.to_string(), name)),
        )
        .collect();
    }
    if !companion.menu.group.is_empty() {
        let labels: &[&str] = if companion.menu.confirming {
            &["Cancel", "Close selected tabs (terminate processes)"]
        } else {
            &[
                "Move to workspace…",
                "Move left",
                "Move right",
                "Close selected tabs…",
                "Clear selection",
            ]
        };
        return labels
            .iter()
            .enumerate()
            .map(|(index, label)| MenuItem::new(index.to_string(), *label))
            .collect();
    }

    let labels: &[&str] = if companion.menu.confirming {
        &["Cancel", "Close workspace (move tabs to fallback)"]
    } else if companion.menu.workspace {
        &["Rename", "Close workspace…"]
    } else {
        &LABELS
    };
    labels
        .iter()
        .enumerate()
        .map(|(index, label)| MenuItem::new(index.to_string(), *label))
        .collect()
}

const fn policy() -> MenuPolicy {
    MenuPolicy::context_menu()
}

fn component<'a>(
    companion: &CompanionState,
    rect: ExtensionRect,
    items: &'a [MenuItem],
    state: &'a Cell<MenuState>,
    outcome: &'a Cell<MenuOutcome>,
) -> impl Component + 'a {
    let color = |value: &str, fallback| {
        let (r, g, b) = super::parse_hex_color(value).unwrap_or(fallback);
        bmux_tui::style::Color::Rgb(r, g, b)
    };
    let local = &companion.local_presentation;
    let fg = color(&local.foreground, (220, 220, 220));
    let bg = color(&local.background, (20, 20, 20));
    let accent = color(&local.status_active, (110, 170, 240));
    let base = Style::new().fg(fg).bg(bg);
    let highlight = Style::new().fg(bg).bg(accent);
    let theme = ModalTheme::new(base, base.fg(accent), base, base, base, highlight);
    let styles = bmux_tui_components::menu::MenuStyles {
        background: base,
        normal: base,
        hovered: highlight,
        pressed: highlight,
        focused: highlight,
        selected: base,
        ..Default::default()
    };
    let menu = MenuComponent::new("item", items, state)
        .policy(policy())
        .styles(styles)
        .fallback_style(base)
        .outcome(outcome);
    let modal = ModalFrame::new(
        ModalSizing::fixed(Size::new(rect.w, rect.h), Insets::new(0, 0, 0, 0)),
        theme,
    )
    .padding(Insets::new(0, 0, 0, 0));
    ModalFrameComponent::new("menu", modal, menu)
}

pub fn viewport(companion: &CompanionState) -> Option<ComponentViewport> {
    companion.menu_tab_id?;
    let rect = popup_rect(companion)?;
    let items = items(companion);
    let state = Cell::new(companion.menu.state);
    let outcome = Cell::new(MenuOutcome::Ignored);
    let component = component(companion, rect, &items, &state, &outcome);
    let layout = component.layout(
        Constraints::tight(Size::new(rect.w, rect.h)),
        &mut LayoutCx::new(),
    );
    ComponentViewport::new(
        layout,
        Rect::new(rect.x, rect.y, rect.w, rect.h),
        Point::new(0, 0),
    )
}

pub fn surfaces(companion: &CompanionState, revision: u64) -> Vec<PluginSurface> {
    let Some(viewport) = viewport(companion) else {
        return Vec::new();
    };
    let rect = viewport.visible_rect();
    let items = items(companion);
    let state = Cell::new(companion.menu.state);
    let outcome = Cell::new(MenuOutcome::Ignored);
    let rect = ExtensionRect::new(rect.x, rect.y, rect.width, rect.height);
    let painted = viewport.paint(&component(companion, rect, &items, &state, &outcome));
    let full = ExtensionRect::new(
        0,
        0,
        companion.local_presentation.viewport_cols,
        companion.local_presentation.viewport_rows,
    );
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
    .interactive_region(PluginSurfaceRegion::new("dismiss", full).endpoint(input_endpoint()));
    backdrop.target = PluginSurfaceTarget::Explicit(full);
    let mut popup = PluginSurface::layout(
        PluginSurfaceId::new(
            OWNER,
            "menu",
            Uuid::from_u128(0x626d_7578_5f74_6162_5f73_7472_6970_0003),
        ),
        revision,
        PluginLayoutId::new(OWNER, LAYOUT_ID),
        bmux_plugin::component_render::buffer_render_ops(&painted.buffer),
    )
    .order(100, 1)
    .opaque(true)
    .modal(true);
    popup.target = PluginSurfaceTarget::Explicit(rect);
    for mut hit in painted.hits {
        hit.rect.x = hit.rect.x.saturating_sub(rect.x);
        hit.rect.y = hit.rect.y.saturating_sub(rect.y);
        // The originating tab retains keyboard ownership through Rename.
        hit.focusable = false;
        popup = popup.interactive_region(hit.endpoint(input_endpoint()));
    }
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
        let (workspace, selector) =
            if let Some(id) = event.hook_id.strip_prefix("bmux.tab_bar:strip:workspace:") {
                (true, id)
            } else {
                (
                    false,
                    event.hook_id.strip_prefix("bmux.tab_bar:strip:tab:")?,
                )
            };
        let target = Uuid::parse_str(selector).ok()?;
        if if workspace {
            companion.workspace_id != Some(target)
        } else {
            !companion.snapshot.tabs.iter().any(|tab| tab.id == target)
        } {
            return None;
        }
        companion.menu = MenuInput {
            workspace,
            ..Default::default()
        };
        if !workspace && companion.multi_selection.contains(&target) {
            companion.menu.group = companion
                .snapshot
                .tabs
                .iter()
                .filter(|tab| companion.multi_selection.contains(&tab.id))
                .map(|tab| tab.id)
                .collect();
        }
        companion.menu_tab_id = Some(target);
        if popup_rect(companion).is_none() {
            companion.menu_tab_id = None;
            return Some(AttachInputResult::default());
        }
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
    if down && event.hook_id == "bmux.tab_bar:menu-dismiss:dismiss" {
        companion.menu_tab_id = None;
        result.release_capture = true;
        result.dirty = true;
        return Some(result);
    }
    // Leave describes the previous hit, not a new pointer position. The paired
    // enter/move carries authoritative coordinates and updates shared hover state.
    if event.phase == "leave" {
        return Some(result);
    }
    let Some(mut input) = bmux_plugin::component_input::component_event(event) else {
        return Some(result);
    };
    if event.hook_id == "bmux.tab_bar:menu-dismiss:dismiss" {
        input = bmux_tui::event::Event::Focus(bmux_tui::event::FocusEvent::Lost);
    }
    let Some(viewport) = companion.menu.geometry.get() else {
        return Some(result);
    };
    let rect = viewport.visible_rect();
    let rect = ExtensionRect::new(rect.x, rect.y, rect.width, rect.height);
    let items = items(companion);
    let state = Cell::new(companion.menu.state);
    let outcome = Cell::new(MenuOutcome::Ignored);
    viewport.event_local(
        &component(companion, rect, &items, &state, &outcome),
        &input,
    );
    result.dirty = state.get() != companion.menu.state;
    companion.menu.state = state.get();
    match outcome.into_inner() {
        MenuOutcome::Cancelled => {
            companion.menu_tab_id = None;
            result.release_capture = true;
            result.dirty = true;
        }
        MenuOutcome::Activated { index, .. } => activate_selection(companion, &mut result, index),
        _ => {}
    }
    Some(result)
}

fn activate_group(
    companion: &mut CompanionState,
    result: &mut AttachInputResult,
    index: usize,
    target: Uuid,
) {
    if !companion.menu.group.is_empty() {
        if companion.menu.confirming || (companion.menu.destinations.is_none() && index != 0) {
            if companion.menu.confirming && index == 0 {
                return;
            }
            if !companion.menu.confirming && index == 4 {
                companion.multi_selection.clear();
                return;
            }
            if !companion.menu.confirming && index == 3 {
                companion.menu.confirming = true;
                companion.menu.state = MenuState::new(Some(0));
                companion.menu_tab_id = Some(target);
                result.release_capture = false;
                return;
            }
            let action = if companion.menu.confirming {
                "close"
            } else if index == 1 {
                "left"
            } else {
                "right"
            };
            result.service_invocation = command_invocation(
                bmux_plugin::AttachInputEndpoint {
                    capability: "bmux.tab_bar.input".into(),
                    interface_id: "presentation-input".into(),
                    operation: "move-selection".into(),
                },
                &MoveSelectionRequest {
                    action: action.into(),
                    tabs: companion.menu.group.clone(),
                    workspace: companion.workspace_id.unwrap_or_default(),
                },
            );
            return;
        }
        if let Some(destinations) = &companion.menu.destinations {
            if index == 1 {
                result.service_invocation = command_invocation(
                    bmux_plugin::AttachInputEndpoint {
                        capability: "bmux.tab_bar.input".into(),
                        interface_id: "presentation-input".into(),
                        operation: "move-selection".into(),
                    },
                    &MoveSelectionRequest {
                        action: "new".into(),
                        tabs: companion.menu.group.clone(),
                        workspace: Uuid::nil(),
                    },
                );
                return;
            }
            if let Some((workspace, _)) = index
                .checked_sub(2)
                .and_then(|index| destinations.get(index))
            {
                result.service_invocation = command_invocation(
                    bmux_plugin::AttachInputEndpoint {
                        capability: "bmux.tab_bar.input".into(),
                        interface_id: "presentation-input".into(),
                        operation: "move-selection".into(),
                    },
                    &MoveSelectionRequest {
                        action: "move".into(),
                        tabs: companion.menu.group.clone(),
                        workspace: *workspace,
                    },
                );
            }
        } else {
            result.service_invocation = command_invocation(
                bmux_plugin::AttachInputEndpoint {
                    capability: "bmux.tab_bar.input".into(),
                    interface_id: "presentation-input".into(),
                    operation: "list-destinations".into(),
                },
                &(),
            );
            if let Some(invocation) = &mut result.service_invocation {
                invocation.response_endpoint = Some(Box::new(input_endpoint()));
            }
            companion.menu.destinations = Some(
                companion
                    .workspace_choices
                    .iter()
                    .filter(|workspace| Some(workspace.id) != companion.workspace_id)
                    .map(|workspace| (workspace.id, workspace.name.clone()))
                    .collect(),
            );
            companion.menu.state = MenuState::new(Some(0));
            companion.menu_tab_id = Some(target);
            result.release_capture = false;
        }
    }
}

fn activate_selection(
    companion: &mut CompanionState,
    result: &mut AttachInputResult,
    index: usize,
) {
    let Some(target) = companion.menu_tab_id.take() else {
        return;
    };
    result.dirty = true;
    result.release_capture = true;
    if !companion.menu.group.is_empty() {
        activate_group(companion, result, index, target);
        return;
    }
    if companion.menu.workspace {
        if companion.workspace_id != Some(target) {
            return;
        }
        if companion.menu.confirming {
            if index == 1 {
                result.service_invocation = command_invocation(
                    bmux_plugin::AttachInputEndpoint {
                        capability: "bmux.tab_bar.input".into(),
                        interface_id: "presentation-input".into(),
                        operation: "close-workspace".into(),
                    },
                    &super::workspaces_commands::client::KillWorkspaceRequest {
                        selector: super::workspaces_state::WorkspaceSelector {
                            id: Some(target),
                            name: None,
                        },
                    },
                );
            }
        } else if index == 0 {
            super::workspace_rename::start_editor(companion, target);
            result.release_capture = false;
            result.capture_keyboard = vec!["*".into()];
        } else {
            companion.menu_tab_id = Some(target);
            companion.menu.confirming = true;
            companion.menu.state = MenuState::new(Some(0));
            result.release_capture = false;
            result.capture_keyboard = vec!["*".into()];
        }
        return;
    }
    result.service_invocation = match index {
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
    use bmux_plugin::RenderOp;

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
        let item = hook
            .strip_prefix("bmux.tab_bar:menu:item:")
            .and_then(|v| v.parse::<u16>().ok());
        AttachInputEvent {
            event_kind: kind.to_string(),
            phase: phase.to_string(),
            key: key.map(str::to_string),
            button: button.map(str::to_string),
            hook_id: hook,
            col: Some(3),
            row: Some(item.map_or(0, |index| index + 2)),
            wheel_delta: 0,
            modifiers: bmux_plugin::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
        }
    }

    fn open(companion: &mut CompanionState) -> AttachInputResult {
        let result = transition(
            companion,
            &event(
                "pointer",
                "down",
                None,
                Some("right"),
                format!("bmux.tab_bar:strip:tab:{}", Uuid::from_u128(7)),
            ),
        )
        .unwrap();
        let viewport = viewport(companion);
        companion.menu.geometry.stage(1, viewport);
        companion.menu.geometry.acknowledge(1);
        result
    }

    fn pointer_event(companion: &CompanionState, phase: &str, item: &str) -> AttachInputEvent {
        let popup = surfaces(companion, 1).pop().unwrap();
        let hit = popup
            .interactive_regions
            .iter()
            .find(|hit| hit.local_id == format!("item.{item}"))
            .unwrap();
        let mut event = event(
            "pointer",
            phase,
            None,
            Some("left"),
            format!("bmux.tab_bar:menu:{}", hit.local_id),
        );
        event.col = Some(hit.rect.x);
        event.row = Some(hit.rect.y);
        event
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
        assert_eq!(companion.menu.state.focused(), Some(1));
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
            companion.menu.state.set_focused(Some(selection));
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
        let down = pointer_event(&companion, "down", "2");
        let up = pointer_event(&companion, "up", "2");
        assert!(
            transition(&mut companion, &down)
                .unwrap()
                .service_invocation
                .is_none()
        );
        let result = transition(&mut companion, &up).unwrap();
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
    fn keyboard_wraps_and_supports_reverse_tab_and_home_end() {
        let mut companion = companion();
        open(&mut companion);
        for (key, shift, expected) in [
            ("up", false, 2),
            ("down", false, 0),
            ("tab", true, 2),
            ("home", false, 0),
            ("end", false, 2),
        ] {
            let mut input = event("key", "press", Some(key), None, String::new());
            input.modifiers.shift = shift;
            assert!(transition(&mut companion, &input).unwrap().consumed);
            assert_eq!(companion.menu.state.focused(), Some(expected));
        }
    }

    #[test]
    fn release_over_another_item_does_not_activate() {
        let mut companion = companion();
        open(&mut companion);
        let down = pointer_event(&companion, "down", "0");
        let up = pointer_event(&companion, "up", "2");
        transition(&mut companion, &down);
        let result = transition(&mut companion, &up).unwrap();
        assert!(result.service_invocation.is_none());
        assert!(companion.menu_tab_id.is_some());
    }

    #[test]
    fn hover_survives_committed_revisions_without_republishing_stationary_pointer() {
        let mut companion = companion();
        open(&mut companion);
        for index in ["1", "2", "1", "0"] {
            let input = pointer_event(&companion, "move", index);
            let before = surfaces(&companion, 1).pop().unwrap().ops;
            assert!(transition(&mut companion, &input).unwrap().dirty);
            let after = surfaces(&companion, 2).pop().unwrap().ops;
            assert_ne!(before, after);
            let selected = index.parse::<usize>().unwrap();
            assert_eq!(companion.menu.state.focused(), Some(selected));
            assert_eq!(companion.menu.state.selected(), Some(selected));
            for (row, label) in LABELS.iter().enumerate() {
                let op = after
                    .iter()
                    .find(|op| matches!(op, RenderOp::TextRun { text, .. } if text.contains(label)))
                    .unwrap();
                let RenderOp::TextRun { text, style, .. } = op else {
                    unreachable!()
                };
                assert_eq!(text.contains('>'), row == selected, "caret on {label}");
                assert_eq!(
                    style.bg,
                    Some(if row == selected {
                        bmux_plugin::RenderColor::Rgb { r: 0, g: 255, b: 0 }
                    } else {
                        bmux_plugin::RenderColor::Rgb { r: 0, g: 0, b: 0 }
                    }),
                    "highlight on {label}"
                );
            }
            let viewport = viewport(&companion);
            companion.menu.geometry.stage(2, viewport);
            companion.menu.geometry.acknowledge(2);
            assert!(!transition(&mut companion, &input).unwrap().dirty);
        }
    }

    #[test]
    fn unpublished_geometry_cannot_receive_input() {
        let mut companion = companion();
        open(&mut companion);
        companion.menu.geometry = CommittedComponentViewport::default();
        let input = pointer_event(&companion, "move", "1");
        assert!(!transition(&mut companion, &input).unwrap().dirty);
        let viewport = viewport(&companion);
        companion.menu.geometry.stage(2, viewport);
        assert!(!transition(&mut companion, &input).unwrap().dirty);
        companion.menu.geometry.acknowledge(2);
        assert!(transition(&mut companion, &input).unwrap().dirty);
    }

    #[test]
    fn hover_remains_live_while_new_publication_is_pending() {
        let mut companion = companion();
        open(&mut companion);
        let first = pointer_event(&companion, "move", "1");
        assert!(transition(&mut companion, &first).unwrap().dirty);
        let viewport = viewport(&companion);
        companion.menu.geometry.stage(2, viewport);
        companion.menu.geometry.acknowledge(1);
        let next = pointer_event(&companion, "move", "2");
        assert!(transition(&mut companion, &next).unwrap().dirty);
        companion.menu.geometry.acknowledge(2);
        assert!(!transition(&mut companion, &next).unwrap().dirty);
    }

    #[test]
    fn focus_loss_removes_menu_and_republishes_without_an_editor() {
        let mut companion = companion();
        open(&mut companion);
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(companion)));
        assert!(crate::handle_focus_lost(&owner, "origin"));
        let companion = owner.lock().unwrap().take().unwrap();
        assert!(companion.menu_tab_id.is_none());
        assert!(companion.menu.geometry.get().is_none());
        let published = companion.surfaces.owner_snapshot(OWNER).unwrap();
        assert!(published.surfaces.iter().all(|surface| !surface.modal));
    }

    #[test]
    fn ctrl_click_toggles_without_switching_and_group_move_captures_ids() {
        let mut companion = companion();
        let id = Uuid::from_u128(7);
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(companion.clone())));
        let mut input = event(
            "pointer",
            "down",
            None,
            Some("left"),
            format!("bmux.tab_bar:strip:tab:{id}"),
        );
        input.modifiers.control = true;
        let result = crate::handle_local_input(&owner, &input).unwrap();
        assert!(result.consumed && result.preserve_focus);
        assert!(result.service_invocation.is_none());
        companion = owner.lock().unwrap().as_ref().unwrap().clone();
        assert!(companion.multi_selection.contains(&id));
        assert!(companion.pointer_source.is_none());
        companion
            .workspace_choices
            .push(crate::workspaces_state::WorkspaceSummary {
                id: Uuid::from_u128(99),
                name: "destination".into(),
                tab_ids: Vec::new(),
                active: false,
            });
        input.modifiers.control = false;
        input.button = Some("right".into());
        transition(&mut companion, &input);
        assert_eq!(companion.menu.group, vec![id]);
        let mut result = AttachInputResult::default();
        activate_selection(&mut companion, &mut result, 0);
        assert_eq!(
            result
                .service_invocation
                .as_ref()
                .unwrap()
                .endpoint
                .operation,
            "list-destinations"
        );
        companion.multi_selection.clear();
        activate_selection(&mut companion, &mut result, 2);
        let invocation = result.service_invocation.unwrap();
        let request: MoveSelectionRequest =
            bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
        assert_eq!(request.tabs, vec![id]);
        assert_eq!(request.workspace, Uuid::from_u128(99));
    }

    #[test]
    fn escape_clears_selection_without_sending_an_action() {
        let mut companion = companion();
        companion.multi_selection.insert(Uuid::from_u128(7));
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(companion)));
        let input = event("shortcut", "press", Some("esc"), None, String::new());
        let result = crate::handle_local_input(&owner, &input).unwrap();
        assert!(result.consumed && result.dirty);
        assert!(result.service_invocation.is_none());
        assert!(
            owner
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .multi_selection
                .is_empty()
        );
        assert!(crate::handle_local_input(&owner, &input).is_none());
    }

    #[test]
    fn group_close_defaults_to_cancel_and_new_destination_is_explicit() {
        let mut companion = companion();
        open(&mut companion);
        companion.menu.group = vec![Uuid::from_u128(7)];
        let mut result = AttachInputResult::default();
        activate_selection(&mut companion, &mut result, 3);
        assert!(companion.menu.confirming);
        assert_eq!(companion.menu.state.selected(), Some(0));
        activate_selection(&mut companion, &mut result, 0);
        assert!(result.service_invocation.is_none());
        open(&mut companion);
        companion.menu.group = vec![Uuid::from_u128(7)];
        companion.menu.destinations = Some(Vec::new());
        assert_eq!(items(&companion).len(), 2);
        activate_selection(&mut companion, &mut result, 1);
        let request: MoveSelectionRequest =
            bmux_plugin_sdk::decode_service_message(&result.service_invocation.unwrap().payload)
                .unwrap();
        assert_eq!(request.action, "new");
    }

    #[test]
    fn alt_click_adds_inclusive_active_range_without_switching() {
        let mut companion = companion();
        let mut snapshot = companion.snapshot.clone();
        snapshot.tabs = (1..=6)
            .map(|id| {
                let mut tab = snapshot.tabs[0].clone();
                tab.id = Uuid::from_u128(id);
                tab.active = id == 3;
                tab
            })
            .collect();
        companion.replace_tabs(snapshot);
        companion.multi_selection.insert(Uuid::from_u128(6));
        let owner = std::sync::Arc::new(std::sync::Mutex::new(Some(companion)));
        for (clicked, expected) in [
            (5, vec![3, 4, 5, 6]),
            (1, vec![1, 2, 3, 4, 5, 6]),
            (3, vec![1, 2, 3, 4, 5, 6]),
        ] {
            let mut input = event(
                "pointer",
                "down",
                None,
                Some("left"),
                format!("bmux.tab_bar:strip:tab:{}", Uuid::from_u128(clicked)),
            );
            input.modifiers.alt = true;
            let result = crate::handle_local_input(&owner, &input).unwrap();
            assert!(result.consumed && result.preserve_focus);
            assert!(result.service_invocation.is_none());
            let state = owner.lock().unwrap().as_ref().unwrap().clone();
            assert_eq!(
                state
                    .multi_selection
                    .iter()
                    .map(Uuid::as_u128)
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                state
                    .snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.active)
                    .unwrap()
                    .id,
                Uuid::from_u128(3)
            );
            assert!(state.pointer_source.is_none());
        }
    }

    #[test]
    fn workspace_menu_renames_and_requires_confirmation_to_close() {
        let mut companion = companion();
        let id = companion.workspace_id.unwrap();
        let input = event(
            "pointer",
            "down",
            None,
            Some("right"),
            format!("bmux.tab_bar:strip:workspace:{id}"),
        );
        assert!(transition(&mut companion, &input).unwrap().consumed);
        assert_eq!(items(&companion).len(), 2);
        let mut result = AttachInputResult::default();
        activate_selection(&mut companion, &mut result, 0);
        assert_eq!(companion.editing_workspace_id, Some(id));
        assert!(companion.menu_tab_id.is_none());
        assert!(transition(&mut companion, &input).unwrap().consumed);
        activate_selection(&mut companion, &mut result, 1);
        assert!(companion.menu.confirming);
        assert!(result.service_invocation.is_none());
        assert_eq!(companion.menu.state.selected(), Some(0));
        activate_selection(&mut companion, &mut result, 0);
        assert!(companion.menu_tab_id.is_none());
        assert!(result.service_invocation.is_none());
        transition(&mut companion, &input);
        activate_selection(&mut companion, &mut result, 1);
        activate_selection(&mut companion, &mut result, 1);
        let invocation = result.service_invocation.unwrap();
        assert_eq!(invocation.endpoint.operation, "close-workspace");
        let request: crate::workspaces_commands::client::KillWorkspaceRequest =
            bmux_plugin_sdk::decode_service_message(&invocation.payload).unwrap();
        assert_eq!(request.selector.id, Some(id));
        assert!(request.selector.name.is_none());
    }

    #[test]
    fn workspace_menu_does_not_retarget_when_workspace_changes() {
        let mut companion = companion();
        let id = companion.workspace_id.unwrap();
        transition(
            &mut companion,
            &event(
                "pointer",
                "down",
                None,
                Some("right"),
                format!("bmux.tab_bar:strip:workspace:{id}"),
            ),
        );
        companion.workspace_id = Some(Uuid::from_u128(99));
        let mut result = AttachInputResult::default();
        activate_selection(&mut companion, &mut result, 1);
        assert!(result.service_invocation.is_none());
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
