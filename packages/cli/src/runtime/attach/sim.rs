use super::adapters::{AttachClock, FixedAttachClock};
use super::input::{TerminalGeometry, TerminalMouseEvent};
#[cfg(test)]
use super::prompt_ui::AttachInternalPromptAction;
use super::prompt_ui::PromptKeyDisposition;
use super::runtime::{
    AttachPointerContinuation, attach_key_event_actions, attach_mouse_forward_bytes_for_target,
    build_attach_help_lines, continue_attach_builtin_pointer_owner, encode_bracketed_paste,
    focused_attach_pane_input_mode, handle_attach_ui_action_at, handle_help_overlay_key_event,
    maybe_begin_attach_mouse_selection_drag, reduce_attach_mouse_floating_drag_event,
    reduce_attach_mouse_resize_event, reduce_attach_status_tab_mouse_event,
    status_row_for_position,
};
use super::state::{
    AttachPointerOwner, AttachTabDropPlacement, AttachUiEffect, AttachViewState, PaneRenderBuffer,
};
use crate::input::{InputProcessor, RuntimeAction};
#[cfg(test)]
use crate::runtime::prompt::PromptRequest;
use crate::status::{AttachStatusLine, build_attach_status_line};
use anyhow::{Result, bail};
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    PaneLayoutNode, PaneState, PaneSummary,
};
use bmux_client::{AttachLayoutState, AttachOpenInfo};
use bmux_config::{BmuxConfig, StatusBarConfig, StatusPosition, StatusTabOrder};
use bmux_keyboard::{KeyCode as BmuxKeyCode, KeyStroke};
use crossterm::event::{
    Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent, KeyEventKind, KeyEventState,
    KeyModifiers,
};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSimWindow {
    pub id: Uuid,
    pub name: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachSimLocatedText {
    pub start_col: u16,
    pub end_col: u16,
    pub center_col: u16,
    pub row: u16,
}

pub struct AttachSimHarness {
    geometry: TerminalGeometry,
    status_config: StatusBarConfig,
    appearance: RuntimeAppearance,
    windows: Vec<AttachSimWindow>,
    view_state: AttachViewState,
    input_processor: InputProcessor,
    effects: Vec<AttachUiEffect>,
    forwarded_bytes: Vec<Vec<u8>>,
    clock: FixedAttachClock,
}

impl AttachSimHarness {
    pub fn new(cols: u16, rows: u16) -> Self {
        let session_id = Uuid::from_u128(1);
        let status_config = StatusBarConfig {
            tab_order: StatusTabOrder::Stable,
            ..StatusBarConfig::default()
        };
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.mouse.tab_drag_enabled = true;
        Self {
            geometry: TerminalGeometry { cols, rows },
            status_config,
            appearance: RuntimeAppearance::default(),
            windows: Vec::new(),
            view_state,
            input_processor: InputProcessor::new(crate::input::Keymap::default_runtime(), false),
            effects: Vec::new(),
            forwarded_bytes: Vec::new(),
            clock: FixedAttachClock::new(Instant::now()),
        }
    }

    pub fn seed_window_list(&mut self, names: &[&str], active: &str) {
        self.windows = names
            .iter()
            .enumerate()
            .map(|(index, name)| AttachSimWindow {
                id: Uuid::from_u128(u128::try_from(index).unwrap_or(0) + 1),
                name: (*name).to_string(),
                active: *name == active,
            })
            .collect();
        self.view_state.attached_context_id = self
            .windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.id);
        self.sync_cached_window_list();
        self.render();
    }

    /// Mirror the simulated window list into `cached_window_list`, matching what
    /// the windows plugin publishes in production so code reading the cache
    /// (such as the inline rename editor) behaves the same under simulation.
    fn sync_cached_window_list(&mut self) {
        use bmux_windows_plugin_api::windows_list::{WindowListEntry, WindowListSnapshot};
        let snapshot = WindowListSnapshot {
            windows: self
                .windows
                .iter()
                .map(|window| WindowListEntry {
                    id: window.id,
                    name: window.name.clone(),
                    active: window.active,
                })
                .collect(),
            revision: 0,
        };
        self.view_state.cached_window_list = Some(std::sync::Arc::new(snapshot));
    }

    pub fn set_tab_order(&mut self, order: StatusTabOrder) {
        self.status_config.tab_order = order;
        self.view_state.mouse.tab_drag_enabled = !matches!(order, StatusTabOrder::Mru);
        self.render();
    }

    pub fn set_status_position(&mut self, position: StatusPosition) {
        self.view_state.status_position = position;
        self.render();
    }

    pub fn resize_viewport(&mut self, cols: u16, rows: u16) {
        self.geometry = TerminalGeometry { cols, rows };
        self.render();
    }

    pub fn set_tab_template(&mut self, template: &str) {
        self.status_config.tab_template = Some(template.to_string());
        self.render();
    }

    /// Text currently shown in the inline tab rename editor, if open.
    pub fn tab_rename_text(&self) -> Option<&str> {
        self.view_state
            .tab_rename
            .as_ref()
            .map(super::state::AttachTabRename::text)
    }

    pub const fn tab_rename_active(&self) -> bool {
        self.view_state.tab_rename.is_some()
    }

    /// Column of the inline editor cursor in the last rendered status line.
    /// Feed a chord (or literal text) into the inline tab rename editor.
    ///
    /// Returns the number of key events applied.
    /// Whether the tab context menu is open.
    pub const fn tab_menu_active(&self) -> bool {
        self.view_state.tab_menu.is_some()
    }

    /// Enabled menu item ids, in display order.
    pub fn tab_menu_items(&self) -> Vec<String> {
        self.view_state
            .tab_menu
            .as_ref()
            .map_or_else(Vec::new, |menu| {
                menu.items
                    .iter()
                    .map(|item| {
                        if item.enabled {
                            item.action.id().to_string()
                        } else {
                            format!("{}:disabled", item.action.id())
                        }
                    })
                    .collect()
            })
    }

    /// Id of the focused menu item.
    pub fn tab_menu_focused(&self) -> Option<String> {
        let menu = self.view_state.tab_menu.as_ref()?;
        menu.items
            .get(menu.focused)
            .map(|item| item.action.id().to_string())
    }

    /// Feed a chord into the open context menu.
    pub fn send_menu_chord(&mut self, chord: &str) -> bool {
        use super::input::TerminalKeyCode;
        let code = match chord {
            "Enter" | "enter" => TerminalKeyCode::Enter,
            "Esc" | "esc" | "Escape" => TerminalKeyCode::Esc,
            "Up" | "up" => TerminalKeyCode::Up,
            "Down" | "down" => TerminalKeyCode::Down,
            "Home" | "home" => TerminalKeyCode::Home,
            "End" | "end" => TerminalKeyCode::End,
            other => {
                let Some(ch) = other.chars().next().filter(|_| other.chars().count() == 1) else {
                    return false;
                };
                TerminalKeyCode::Char(ch)
            }
        };
        let key = super::input::TerminalKeyEvent {
            code,
            modifiers: super::input::TerminalModifiers::default(),
            kind: super::input::TerminalKeyPhase::Press,
        };
        let Some(reduction) =
            super::runtime::handle_attach_tab_menu_key_event(&mut self.view_state, &key)
        else {
            return false;
        };
        for effect in reduction.effects {
            self.apply_effect(effect);
        }
        self.render();
        true
    }

    pub fn send_rename_chord(&mut self, chord: &str) -> usize {
        use super::input::TerminalKeyCode;

        let code = match chord {
            "Enter" | "enter" => Some(TerminalKeyCode::Enter),
            "Esc" | "esc" | "Escape" => Some(TerminalKeyCode::Esc),
            "Backspace" | "backspace" => Some(TerminalKeyCode::Backspace),
            "Delete" | "delete" => Some(TerminalKeyCode::Delete),
            "Left" | "left" => Some(TerminalKeyCode::Left),
            "Right" | "right" => Some(TerminalKeyCode::Right),
            "Home" | "home" => Some(TerminalKeyCode::Home),
            "End" | "end" => Some(TerminalKeyCode::End),
            _ => None,
        };
        let mut applied = 0usize;
        if let Some(code) = code {
            self.send_rename_key(&super::input::TerminalKeyEvent {
                code,
                modifiers: super::input::TerminalModifiers::default(),
                kind: super::input::TerminalKeyPhase::Press,
            });
            return 1;
        }
        // Otherwise treat the chord as literal text to type.
        for ch in chord.chars() {
            self.send_rename_key(&super::input::TerminalKeyEvent {
                code: TerminalKeyCode::Char(ch),
                modifiers: super::input::TerminalModifiers::default(),
                kind: super::input::TerminalKeyPhase::Press,
            });
            applied += 1;
        }
        applied
    }

    pub fn send_rename_key(&mut self, key: &super::input::TerminalKeyEvent) {
        if let Some(reduction) =
            super::runtime::handle_attach_tab_rename_key_event(&mut self.view_state, key)
        {
            for effect in reduction.effects {
                self.apply_effect(effect);
            }
            self.render();
        }
    }

    pub fn render(&mut self) -> &AttachStatusLine {
        let mode_label = if self.view_state.help_overlay_open {
            "HELP"
        } else if self.view_state.prompt.is_active() {
            "PROMPT"
        } else {
            "NORMAL"
        };
        let hint = if self.view_state.help_overlay_open {
            "Help overlay open | ? toggles | Esc/Enter close"
        } else {
            self.view_state.prompt.active_hint().unwrap_or("")
        };
        let status_line = build_attach_status_line(
            self.geometry.cols,
            &self.status_config,
            &self.appearance,
            "sim",
            1,
            "sim",
            mode_label,
            "write",
            None,
            self.view_state
                .focused_scrollback()
                .and_then(|view| view.pin.map(|_| "FROZEN")),
            hint,
        );
        self.view_state.cached_status_line = Some(status_line);
        self.view_state
            .cached_status_line
            .as_ref()
            .expect("simulation render should cache status line")
    }

    #[cfg(test)]
    pub const fn set_clock(&mut self, now: Instant) {
        self.clock.set_now(now);
    }

    #[cfg(test)]
    pub fn advance_clock(&mut self, duration: std::time::Duration) {
        self.clock.advance(duration);
    }

    pub fn send_mouse(&mut self, event: TerminalMouseEvent) {
        if !self.view_state.mouse.config.enabled
            || self.view_state.help_overlay_open
            || self.view_state.prompt.is_active()
        {
            self.view_state.mouse.clear_pointer_gestures();
            return;
        }
        if !self.view_state.can_write {
            self.view_state.mouse.clear_mutation_pointer_gestures();
        }

        // An open context menu owns the pointer, matching production ordering.
        if self.view_state.tab_menu.is_some()
            && let Some(reduction) = super::runtime::handle_attach_tab_menu_mouse_event(
                &mut self.view_state,
                event,
                self.geometry,
            )
        {
            for effect in reduction.effects {
                self.apply_effect(effect);
            }
            self.render();
            return;
        }

        let mut reduction = match self.view_state.mouse.pointer_owner() {
            Some(AttachPointerOwner::Plugin) => return,
            _ => match continue_attach_builtin_pointer_owner(
                &mut self.view_state,
                event,
                self.clock.now(),
                self.geometry,
            ) {
                AttachPointerContinuation::Owned(reduction) => reduction,
                AttachPointerContinuation::Unowned => reduce_attach_status_tab_mouse_event(
                    &mut self.view_state,
                    event,
                    self.geometry,
                    self.clock.now(),
                ),
            },
        };
        if !reduction.consumed {
            if self.try_forward_mouse(event) {
                return;
            }
            if !self.view_state.can_write {
                return;
            }
            reduction =
                reduce_attach_mouse_resize_event(&mut self.view_state, event, self.clock.now());
        }
        if !reduction.consumed {
            reduction = reduce_attach_mouse_floating_drag_event(&mut self.view_state, event);
        }
        if !reduction.consumed
            && matches!(
                (event.phase, event.button),
                (
                    super::input::TerminalMousePhase::Down,
                    Some(super::input::TerminalMouseButton::Left)
                )
            )
            && let Some(mouse_event) = event.to_crossterm()
        {
            let target = self.mouse_content_target(event);
            if maybe_begin_attach_mouse_selection_drag(&mut self.view_state, target, mouse_event) {
                reduction = super::state::AttachUiReduction::consumed();
            }
        }
        if !reduction.consumed {
            return;
        }
        for effect in reduction.effects {
            self.apply_effect(effect);
        }
        self.render();
    }

    fn mouse_content_target(&self, event: TerminalMouseEvent) -> Option<Uuid> {
        self.view_state
            .cached_layout_state
            .as_ref()
            .and_then(|layout| {
                layout
                    .scene
                    .surfaces
                    .iter()
                    .rev()
                    .find(|surface| {
                        surface.visible
                            && surface.accepts_input
                            && event.col >= surface.rect.x
                            && event.col < surface.rect.x.saturating_add(surface.rect.w)
                            && event.row >= surface.rect.y
                            && event.row < surface.rect.y.saturating_add(surface.rect.h)
                    })
                    .filter(|surface| {
                        event.col >= surface.content_rect.x
                            && event.col
                                < surface
                                    .content_rect
                                    .x
                                    .saturating_add(surface.content_rect.w)
                            && event.row >= surface.content_rect.y
                            && event.row
                                < surface
                                    .content_rect
                                    .y
                                    .saturating_add(surface.content_rect.h)
                    })
                    .and_then(|surface| surface.pane_id)
            })
    }

    fn try_forward_mouse(&mut self, event: TerminalMouseEvent) -> bool {
        if self.view_state.mouse.pointer_owner().is_some() {
            return false;
        }
        let Some(mouse_event) = event.to_crossterm() else {
            return false;
        };
        let target = self.mouse_content_target(event);
        let Some(bytes) = attach_mouse_forward_bytes_for_target(
            &self.view_state,
            mouse_event,
            target,
            target.is_some(),
        ) else {
            return false;
        };
        self.forwarded_bytes.push(bytes);
        true
    }

    #[cfg(test)]
    pub fn pointer_owner(&self) -> Option<AttachPointerOwner> {
        self.view_state.mouse.pointer_owner()
    }

    #[cfg(test)]
    pub const fn set_bracketed_paste_enabled(&mut self, enabled: bool) {
        self.view_state.bracketed_paste_enabled = enabled;
    }

    #[cfg(test)]
    pub fn set_pane_bracketed_paste(&mut self, pane_id: Uuid, enabled: bool) {
        self.view_state.pane_input_mode_hints.insert(
            pane_id,
            bmux_attach_layout_protocol::AttachInputModeState {
                bracketed_paste: enabled,
                ..bmux_attach_layout_protocol::AttachInputModeState::default()
            },
        );
    }

    #[cfg(test)]
    pub fn focus_pane(&mut self, pane_id: Uuid) {
        self.apply_effect(AttachUiEffect::FocusPane { pane_id });
    }

    #[cfg(test)]
    pub fn send_paste(&mut self, text: &str) -> bool {
        if !self.view_state.bracketed_paste_enabled
            || !self.view_state.can_write
            || self.view_state.help_overlay_open
            || self.view_state.prompt.is_active()
        {
            return false;
        }
        let mode = focused_attach_pane_input_mode(&self.view_state);
        self.forwarded_bytes
            .push(encode_bracketed_paste(text, mode.bracketed_paste));
        true
    }

    #[cfg(test)]
    pub fn forwarded_mouse_bytes(&self) -> &[Vec<u8>] {
        &self.forwarded_bytes
    }

    #[cfg(test)]
    pub fn open_text_prompt(&mut self) {
        self.view_state.prompt.enqueue_internal(
            PromptRequest::text_input("Value"),
            AttachInternalPromptAction::QuitSession,
        );
    }

    #[cfg(test)]
    pub fn paste_into_prompt(&mut self, text: &str) -> PromptKeyDisposition {
        let disposition = self.view_state.prompt.handle_paste(text);
        if matches!(disposition, PromptKeyDisposition::Consumed) {
            self.view_state
                .dirty
                .mark_overlay_dirty(super::state::AttachDirtySource::PromptOverlay);
        }
        disposition
    }

    #[cfg(test)]
    pub const fn prompt_overlay_dirty(&self) -> bool {
        self.view_state.dirty.overlay_needs_redraw
    }

    #[cfg(test)]
    pub fn open_help_overlay(&mut self) {
        self.view_state.help_overlay_open = true;
        self.view_state.mouse.clear_pointer_gestures();
    }

    #[cfg(test)]
    pub fn set_can_write(&mut self, can_write: bool) {
        self.view_state.can_write = can_write;
        if !can_write {
            self.view_state.mouse.clear_mutation_pointer_gestures();
        }
    }

    #[cfg(test)]
    pub fn disable_mouse(&mut self) {
        self.view_state.mouse.config.enabled = false;
        self.view_state.mouse.clear_pointer_gestures();
    }

    #[cfg(test)]
    pub fn enable_pane_mouse_reporting(&mut self) {
        let Some(layout) = self.view_state.cached_layout_state.as_ref() else {
            return;
        };
        for pane in &layout.panes {
            self.view_state.pane_mouse_protocol_hints.insert(
                pane.id,
                bmux_attach_layout_protocol::AttachMouseProtocolState {
                    mode: bmux_attach_layout_protocol::AttachMouseProtocolMode::AnyMotion,
                    encoding: bmux_attach_layout_protocol::AttachMouseProtocolEncoding::Sgr,
                },
            );
        }
    }

    pub fn seed_vertical_split_panes(&mut self) {
        let left_pane = Uuid::from_u128(21);
        let right_pane = Uuid::from_u128(22);
        let height = self.geometry.rows.saturating_sub(1).max(4);
        self.view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: self.view_state.attached_context_id,
            session_id: self.view_state.attached_id,
            focused_pane_id: left_pane,
            panes: vec![
                PaneSummary {
                    id: left_pane,
                    index: 1,
                    name: Some("left".to_string()),
                    focused: true,
                    state: PaneState::Running,
                    state_reason: None,
                },
                PaneSummary {
                    id: right_pane,
                    index: 2,
                    name: Some("right".to_string()),
                    focused: false,
                    state: PaneState::Running,
                    state_reason: None,
                },
            ],
            layout_root: PaneLayoutNode::Split {
                direction: bmux_attach_layout_protocol::PaneSplitDirection::Vertical,
                ratio_percent: 50,
                first: Box::new(PaneLayoutNode::Leaf { pane_id: left_pane }),
                second: Box::new(PaneLayoutNode::Leaf {
                    pane_id: right_pane,
                }),
            },
            scene: AttachScene {
                session_id: self.view_state.attached_id,
                focus: AttachFocusTarget::Pane { pane_id: left_pane },
                surfaces: vec![
                    AttachSurface {
                        id: Uuid::from_u128(23),
                        kind: AttachSurfaceKind::Pane,
                        layer: AttachLayer::Pane,
                        z: 0,
                        pane_id: Some(left_pane),
                        rect: AttachRect {
                            x: 0,
                            y: 0,
                            w: 10,
                            h: height,
                        },
                        content_rect: AttachRect {
                            x: 1,
                            y: 1,
                            w: 8,
                            h: height.saturating_sub(2).max(1),
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: true,
                    },
                    AttachSurface {
                        id: Uuid::from_u128(24),
                        kind: AttachSurfaceKind::Pane,
                        layer: AttachLayer::Pane,
                        z: 0,
                        pane_id: Some(right_pane),
                        rect: AttachRect {
                            x: 10,
                            y: 0,
                            w: 10,
                            h: height,
                        },
                        content_rect: AttachRect {
                            x: 11,
                            y: 1,
                            w: 8,
                            h: height.saturating_sub(2).max(1),
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: false,
                    },
                ],
            },
            zoomed: false,
        });
    }

    pub fn seed_floating_pane_layout(&mut self) {
        let tiled_pane = Uuid::from_u128(31);
        let floating_pane = Uuid::from_u128(32);
        let height = self.geometry.rows.saturating_sub(1).max(8);
        self.view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: self.view_state.attached_context_id,
            session_id: self.view_state.attached_id,
            focused_pane_id: tiled_pane,
            panes: vec![
                PaneSummary {
                    id: tiled_pane,
                    index: 1,
                    name: Some("tiled".to_string()),
                    focused: true,
                    state: PaneState::Running,
                    state_reason: None,
                },
                PaneSummary {
                    id: floating_pane,
                    index: 2,
                    name: Some("float".to_string()),
                    focused: false,
                    state: PaneState::Running,
                    state_reason: None,
                },
            ],
            layout_root: PaneLayoutNode::Leaf {
                pane_id: tiled_pane,
            },
            scene: AttachScene {
                session_id: self.view_state.attached_id,
                focus: AttachFocusTarget::Pane {
                    pane_id: tiled_pane,
                },
                surfaces: vec![
                    AttachSurface {
                        id: Uuid::from_u128(33),
                        kind: AttachSurfaceKind::Pane,
                        layer: AttachLayer::Pane,
                        z: 0,
                        pane_id: Some(tiled_pane),
                        rect: AttachRect {
                            x: 0,
                            y: 0,
                            w: 40,
                            h: height,
                        },
                        content_rect: AttachRect {
                            x: 1,
                            y: 1,
                            w: 38,
                            h: height.saturating_sub(2).max(1),
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: true,
                    },
                    AttachSurface {
                        id: Uuid::from_u128(34),
                        kind: AttachSurfaceKind::FloatingPane,
                        layer: AttachLayer::FloatingPane,
                        z: 10,
                        pane_id: Some(floating_pane),
                        rect: AttachRect {
                            x: 2,
                            y: 2,
                            w: 10,
                            h: 6,
                        },
                        content_rect: AttachRect {
                            x: 3,
                            y: 3,
                            w: 8,
                            h: 4,
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: false,
                    },
                ],
            },
            zoomed: false,
        });
    }

    pub fn seed_pane_lines(&mut self, lines: &[&str], cursor_row: u16, cursor_col: u16) {
        let pane_id = Uuid::from_u128(10);
        let content_width = lines
            .iter()
            .map(|line| u16::try_from(line.chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(1)
            .max(1);
        let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
        let outer_width = content_width
            .saturating_add(2)
            .min(self.geometry.cols.max(2));
        let outer_height = content_height
            .saturating_add(2)
            .min(self.geometry.rows.max(2));
        self.view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: self.view_state.attached_context_id,
            session_id: self.view_state.attached_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id: self.view_state.attached_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::from_u128(11),
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: outer_width,
                        h: outer_height,
                    },
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: outer_width.saturating_sub(2).max(1),
                        h: outer_height.saturating_sub(2).max(1),
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });
        let buffer = self
            .view_state
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    content_width,
                    content_height,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("attach-sim pane grid dimensions are valid"),
                ..PaneRenderBuffer::default()
            });
        buffer.terminal_grid = bmux_terminal_grid::TerminalGridStream::new(
            content_width,
            content_height,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("attach-sim pane grid dimensions are valid");
        buffer.visual_row_fingerprints.clear();
        let mut bytes = lines.join("\r\n").into_bytes();
        bytes.extend_from_slice(format!("\x1b[{cursor_row};{cursor_col}H").as_bytes());
        append_sim_pane_output(buffer, &bytes);
        // Reseeding one pane's content invalidates only that pane's view.
        self.view_state.exit_scrollback_for(pane_id);
    }

    pub fn send_attach_chord(&mut self, chord: &str) -> Result<Vec<String>> {
        self.input_processor
            .set_scroll_mode(self.view_state.scrollback_active());
        let strokes = crate::input::parse_key_chord(chord)
            .map_err(|error| anyhow::anyhow!("invalid attach key chord '{chord}': {error}"))?;
        let mut emitted = Vec::new();
        for stroke in strokes {
            let event = crossterm_event_from_stroke(stroke);
            let CrosstermEvent::Key(key) = event else {
                continue;
            };
            if self.view_state.prompt.is_active() {
                match self.view_state.prompt.handle_key_event(&key) {
                    PromptKeyDisposition::Completed(_) => {
                        emitted.push("prompt:completed".to_string());
                    }
                    PromptKeyDisposition::Consumed => emitted.push("prompt:consumed".to_string()),
                    PromptKeyDisposition::NotActive => {}
                }
                continue;
            }
            if self.view_state.help_overlay_open {
                let help_lines = build_attach_help_lines(&BmuxConfig::default());
                if handle_help_overlay_key_event(
                    &key,
                    &help_lines,
                    &mut self.view_state,
                    self.geometry,
                ) {
                    emitted.push("help:handled".to_string());
                    continue;
                }
            }
            for action in
                attach_key_event_actions(&key, &mut self.input_processor, self.view_state.ui_mode)?
            {
                self.apply_attach_event_action(action, &mut emitted)?;
            }
        }
        let trailing = self.input_processor.process_stream_bytes(&[]);
        for ui_action in trailing {
            emitted.push(format!("ui:{ui_action:?}"));
            self.apply_ui_action(&ui_action);
        }
        self.render();
        Ok(emitted)
    }

    fn apply_attach_event_action(
        &mut self,
        action: super::state::AttachEventAction,
        emitted: &mut Vec<String>,
    ) -> Result<()> {
        match action {
            super::state::AttachEventAction::Ui(ui_action) => {
                emitted.push(format!("ui:{ui_action:?}"));
                self.apply_ui_action(&ui_action);
            }
            super::state::AttachEventAction::Send(bytes) => {
                emitted.push("send".to_string());
                self.forwarded_bytes.push(bytes);
            }
            super::state::AttachEventAction::Paste(text) => {
                emitted.push("paste".to_string());
                let mode = focused_attach_pane_input_mode(&self.view_state);
                self.forwarded_bytes
                    .push(encode_bracketed_paste(&text, mode.bracketed_paste));
            }
            super::state::AttachEventAction::Ignore => {
                emitted.push("ignore".to_string());
            }
            super::state::AttachEventAction::Detach => {
                bail!("attach-sim send-attach emitted detach");
            }
            super::state::AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } => {
                bail!(
                    "attach-sim send-attach emitted unsupported plugin command {plugin_id}:{command_name}"
                );
            }
            super::state::AttachEventAction::Mouse(_) | super::state::AttachEventAction::Redraw => {
            }
        }
        Ok(())
    }

    fn apply_ui_action(&mut self, ui_action: &RuntimeAction) {
        if matches!(ui_action, RuntimeAction::ShowHelp) {
            self.view_state.help_overlay_open = !self.view_state.help_overlay_open;
            if self.view_state.help_overlay_open {
                self.view_state.mouse.clear_pointer_gestures();
            } else {
                self.view_state.help_overlay_scroll = 0;
            }
        } else {
            handle_attach_ui_action_at(ui_action, &mut self.view_state, self.clock.now());
        }
    }

    pub fn rendered(&self) -> &str {
        self.view_state
            .cached_status_line
            .as_ref()
            .map_or("", |status_line| status_line.rendered.as_str())
    }

    pub fn effects(&self) -> &[AttachUiEffect] {
        &self.effects
    }

    pub fn window_names(&self) -> Vec<String> {
        self.windows
            .iter()
            .map(|window| window.name.clone())
            .collect()
    }

    pub fn active_window_name(&self) -> Option<&str> {
        self.windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.name.as_str())
    }

    /// Rendered text of each visible tab, taken from its hitbox columns so the
    /// result reflects the resolved tab template.
    pub fn rendered_tab_labels(&self) -> Vec<String> {
        let Some(status_line) = self.view_state.cached_status_line.as_ref() else {
            return Vec::new();
        };
        let plain = status_line
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        let cells = plain.chars().collect::<Vec<_>>();
        let mut hitboxes = status_line.tab_hitboxes.iter().collect::<Vec<_>>();
        hitboxes.sort_by_key(|hitbox| hitbox.start_col);
        hitboxes
            .into_iter()
            .filter_map(|hitbox| {
                let start = usize::from(hitbox.start_col);
                let end = usize::from(hitbox.end_col);
                cells
                    .get(start..=end)
                    .map(|token| token.iter().collect::<String>().trim().to_string())
            })
            .collect()
    }

    pub fn scrollback_active(&self) -> bool {
        self.view_state.scrollback_active()
    }

    pub fn selection_active(&self) -> bool {
        self.view_state.selection_active()
    }

    pub const fn help_overlay_open(&self) -> bool {
        self.view_state.help_overlay_open
    }

    pub const fn help_overlay_scroll(&self) -> usize {
        self.view_state.help_overlay_scroll
    }

    pub const fn prompt_active(&self) -> bool {
        self.view_state.prompt.is_active()
    }

    pub fn selected_text(&mut self) -> Option<String> {
        super::runtime::selected_attach_text(&mut self.view_state)
    }

    pub fn scrollback_cursor(&self) -> Option<(usize, usize)> {
        self.view_state
            .focused_scrollback()
            .map(|view| (view.cursor.row, view.cursor.col))
    }

    pub fn locate_text(&self, text: &str) -> Option<AttachSimLocatedText> {
        let status_line = self.view_state.cached_status_line.as_ref()?;
        for (index, window) in self.windows.iter().enumerate() {
            // Accept either the bare window name or the legacy indexed form so
            // fixtures keep working across tab-template changes.
            let indexed_label = format!("{}:{}", index + 1, window.name);
            if text == indexed_label || text == window.name {
                let hitbox = status_line
                    .tab_hitboxes
                    .iter()
                    .find(|hitbox| hitbox.context_id == window.id)?;
                return Some(AttachSimLocatedText {
                    start_col: hitbox.start_col,
                    end_col: hitbox.end_col,
                    center_col: hitbox
                        .start_col
                        .saturating_add(hitbox.end_col.saturating_sub(hitbox.start_col) / 2),
                    row: status_row_for_position(
                        self.view_state.status_position,
                        self.geometry.rows,
                    )?,
                });
            }
        }
        let rendered = self.rendered();
        let start = rendered.find(text)?;
        let end = start.checked_add(text.len())?.checked_sub(1)?;
        let start_col = u16::try_from(start).ok()?;
        let end_col = u16::try_from(end).ok()?;
        Some(AttachSimLocatedText {
            start_col,
            end_col,
            center_col: start_col.saturating_add(end_col.saturating_sub(start_col) / 2),
            row: status_row_for_position(self.view_state.status_position, self.geometry.rows)?,
        })
    }

    fn apply_effect(&mut self, effect: AttachUiEffect) {
        match effect.clone() {
            AttachUiEffect::SwitchWindow { target_context_id } => {
                for window in &mut self.windows {
                    window.active = window.id == target_context_id;
                }
                self.view_state.attached_context_id = Some(target_context_id);
            }
            AttachUiEffect::MoveWindow {
                source_context_id,
                target_context_id,
                placement,
            } => {
                reorder_windows(
                    &mut self.windows,
                    source_context_id,
                    target_context_id,
                    placement,
                );
                self.sync_cached_window_list();
            }
            AttachUiEffect::RenameWindow { context_id, name } => {
                if let Some(window) = self.windows.iter_mut().find(|w| w.id == context_id) {
                    window.name = name;
                }
                self.sync_cached_window_list();
            }
            AttachUiEffect::CloseWindow { context_id } => {
                self.windows.retain(|window| window.id != context_id);
                if !self.windows.iter().any(|window| window.active)
                    && let Some(first) = self.windows.first_mut()
                {
                    first.active = true;
                }
                self.view_state.attached_context_id = self
                    .windows
                    .iter()
                    .find(|window| window.active)
                    .map(|window| window.id);
                self.sync_cached_window_list();
            }
            AttachUiEffect::NewWindow => {
                let id = Uuid::from_u128(
                    u128::try_from(self.windows.len())
                        .unwrap_or(0)
                        .saturating_add(1000),
                );
                for window in &mut self.windows {
                    window.active = false;
                }
                self.windows.push(AttachSimWindow {
                    id,
                    name: format!("tab-{}", self.windows.len().saturating_add(1)),
                    active: true,
                });
                self.view_state.attached_context_id = Some(id);
                self.sync_cached_window_list();
            }
            AttachUiEffect::ResizePane { .. } | AttachUiEffect::ShowTransientStatus { .. } => {}
            AttachUiEffect::FocusPane { pane_id } => {
                if let Some(layout_state) = &mut self.view_state.cached_layout_state {
                    layout_state.focused_pane_id = pane_id;
                    layout_state.scene.focus = AttachFocusTarget::Pane { pane_id };
                    for pane in &mut layout_state.panes {
                        pane.focused = pane.id == pane_id;
                    }
                }
                self.view_state.mouse.last_focused_pane_id = Some(pane_id);
            }
            AttachUiEffect::MoveFloatingPane { pane_id, x, y } => {
                if let Some(layout_state) = &mut self.view_state.cached_layout_state {
                    for surface in &mut layout_state.scene.surfaces {
                        if surface.pane_id == Some(pane_id)
                            && surface.kind == AttachSurfaceKind::FloatingPane
                        {
                            let inner_x_offset =
                                surface.content_rect.x.saturating_sub(surface.rect.x);
                            let inner_y_offset =
                                surface.content_rect.y.saturating_sub(surface.rect.y);
                            surface.rect.x = x;
                            surface.rect.y = y;
                            surface.content_rect.x = x.saturating_add(inner_x_offset);
                            surface.content_rect.y = y.saturating_add(inner_y_offset);
                        }
                    }
                }
            }
        }
        self.effects.push(effect);
    }
}

fn append_sim_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) {
    let was_alternate = buffer.protocol_tracker.alternate_screen();
    let previous_content_revision = buffer.terminal_grid.grid().content_revision();
    let _ = buffer.protocol_tracker.process(bytes);
    buffer.terminal_grid.process(bytes);
    if buffer.terminal_grid.grid().content_revision() != previous_content_revision {
        buffer.visual_row_fingerprints.clear();
    }
    if was_alternate != buffer.protocol_tracker.alternate_screen() {
        buffer.prev_rows.clear();
    }
}

fn crossterm_event_from_stroke(stroke: KeyStroke) -> CrosstermEvent {
    let code = match stroke.key {
        BmuxKeyCode::Char(value) => CrosstermKeyCode::Char(value),
        BmuxKeyCode::Enter => CrosstermKeyCode::Enter,
        BmuxKeyCode::Tab => CrosstermKeyCode::Tab,
        BmuxKeyCode::Backspace => CrosstermKeyCode::Backspace,
        BmuxKeyCode::Delete => CrosstermKeyCode::Delete,
        BmuxKeyCode::Escape => CrosstermKeyCode::Esc,
        BmuxKeyCode::Space => CrosstermKeyCode::Char(' '),
        BmuxKeyCode::Up => CrosstermKeyCode::Up,
        BmuxKeyCode::Down => CrosstermKeyCode::Down,
        BmuxKeyCode::Left => CrosstermKeyCode::Left,
        BmuxKeyCode::Right => CrosstermKeyCode::Right,
        BmuxKeyCode::Home => CrosstermKeyCode::Home,
        BmuxKeyCode::End => CrosstermKeyCode::End,
        BmuxKeyCode::PageUp => CrosstermKeyCode::PageUp,
        BmuxKeyCode::PageDown => CrosstermKeyCode::PageDown,
        BmuxKeyCode::Insert => CrosstermKeyCode::Insert,
        BmuxKeyCode::F(value) => CrosstermKeyCode::F(value),
    };

    let mut modifiers = KeyModifiers::NONE;
    if stroke.modifiers.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if stroke.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if stroke.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if stroke.modifiers.super_key {
        modifiers |= KeyModifiers::SUPER;
    }
    if stroke.modifiers.hyper {
        modifiers |= KeyModifiers::HYPER;
    }
    if stroke.modifiers.meta {
        modifiers |= KeyModifiers::META;
    }

    CrosstermEvent::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn reorder_windows(
    windows: &mut Vec<AttachSimWindow>,
    source_context_id: Uuid,
    target_context_id: Uuid,
    placement: AttachTabDropPlacement,
) {
    if source_context_id == target_context_id {
        return;
    }
    let Some(source_index) = windows
        .iter()
        .position(|window| window.id == source_context_id)
    else {
        return;
    };
    let source = windows.remove(source_index);
    let Some(mut target_index) = windows
        .iter()
        .position(|window| window.id == target_context_id)
    else {
        windows.insert(source_index.min(windows.len()), source);
        return;
    };
    if matches!(placement, AttachTabDropPlacement::After) {
        target_index = target_index.saturating_add(1);
    }
    windows.insert(target_index.min(windows.len()), source);
}

#[cfg(test)]
mod tests {
    use super::AttachSimHarness;
    use crate::runtime::attach::input::{
        TerminalModifiers, TerminalMouseButton, TerminalMouseEvent, TerminalMousePhase,
    };
    use crate::runtime::attach::state::{AttachPointerOwner, AttachUiEffect};
    use bmux_attach_layout_protocol::AttachFocusTarget;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    const fn left_mouse(phase: TerminalMousePhase, col: u16, row: u16) -> TerminalMouseEvent {
        TerminalMouseEvent {
            phase,
            button: Some(TerminalMouseButton::Left),
            col,
            row,
            modifiers: TerminalModifiers {
                shift: false,
                control: false,
                alt: false,
                super_key: false,
                hyper: false,
                meta: false,
            },
        }
    }

    #[test]
    fn attach_sim_active_prompt_consumes_paste_and_marks_redraw() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.set_bracketed_paste_enabled(true);
        sim.open_text_prompt();

        assert!(matches!(
            sim.paste_into_prompt("prompt text"),
            crate::runtime::attach::prompt_ui::PromptKeyDisposition::Consumed
        ));
        assert!(sim.prompt_overlay_dirty());
        assert!(!sim.send_paste("must not reach pane"));
        assert!(sim.forwarded_mouse_bytes().is_empty());
    }

    #[test]
    fn attach_sim_routes_paste_by_focused_pane_mode_and_transitions() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.set_bracketed_paste_enabled(true);
        let layout = sim
            .view_state
            .cached_layout_state
            .as_ref()
            .expect("split layout");
        let left = layout.panes[0].id;
        let right = layout.panes[1].id;
        sim.set_pane_bracketed_paste(left, false);
        sim.set_pane_bracketed_paste(right, true);

        assert!(sim.send_paste("left"));
        sim.focus_pane(right);
        assert!(sim.send_paste("right"));
        sim.set_pane_bracketed_paste(right, false);
        assert!(sim.send_paste("reset"));

        assert_eq!(
            sim.forwarded_mouse_bytes(),
            &[
                b"left".to_vec(),
                b"\x1b[200~right\x1b[201~".to_vec(),
                b"reset".to_vec(),
            ]
        );
    }

    #[test]
    fn attach_sim_paste_respects_capability_permissions_and_overlays() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.set_bracketed_paste_enabled(false);
        assert!(!sim.send_paste("disabled"));

        sim.set_bracketed_paste_enabled(true);
        sim.set_can_write(false);
        assert!(!sim.send_paste("read-only"));

        sim.set_can_write(true);
        assert!(sim.send_paste("writable-again"));
        sim.open_help_overlay();
        assert!(!sim.send_paste("overlay"));
        assert_eq!(sim.forwarded_mouse_bytes(), &[b"writable-again".to_vec()]);
    }

    #[test]
    fn attach_sim_paste_payload_is_opaque_for_edge_cases() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.set_bracketed_paste_enabled(true);
        let pane = match sim
            .view_state
            .cached_layout_state
            .as_ref()
            .expect("split layout")
            .scene
            .focus
        {
            AttachFocusTarget::Pane { pane_id } => pane_id,
            AttachFocusTarget::Surface { .. } | AttachFocusTarget::None => {
                panic!("expected pane focus")
            }
        };
        sim.set_pane_bracketed_paste(pane, true);
        let large = "x".repeat(128 * 1024);
        let payloads = [
            String::new(),
            "one\ntwo\r\n世界\0\x03".to_string(),
            "\x1b[200~nested\x1b[201~".to_string(),
            large,
        ];

        for payload in &payloads {
            assert!(sim.send_paste(payload));
        }
        for (sent, payload) in sim.forwarded_mouse_bytes().iter().zip(&payloads) {
            assert!(sent.starts_with(b"\x1b[200~"));
            assert!(sent.ends_with(b"\x1b[201~"));
            assert_eq!(&sent[6..sent.len() - 6], payload.as_bytes());
        }
    }

    #[test]
    fn attach_sim_uses_fixed_clock_adapter() {
        let start = Instant::now();
        let mut sim = AttachSimHarness::new(100, 24);
        sim.set_clock(start);
        sim.advance_clock(Duration::from_millis(10));
        sim.seed_vertical_split_panes();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 10, 5));

        assert!(sim.effects().is_empty());
    }

    fn right_mouse(phase: TerminalMousePhase, col: u16, row: u16) -> TerminalMouseEvent {
        TerminalMouseEvent {
            phase,
            button: Some(super::super::input::TerminalMouseButton::Right),
            col,
            row,
            modifiers: TerminalModifiers::default(),
        }
    }

    #[test]
    fn attach_sim_right_click_off_the_strip_is_ignored() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        // Right-click in pane content: bmux must not open a tab menu there.
        sim.send_mouse(right_mouse(TerminalMousePhase::Down, 10, 5));
        assert!(!sim.tab_menu_active());
    }

    #[test]
    fn attach_sim_send_attach_drives_scrollback_selection() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_pane_lines(&["one", "  four", "     five", "  six"], 4, 3);

        sim.send_attach_chord("ctrl+a [").expect("enter scrollback");
        assert!(sim.scrollback_active());
        assert_eq!(sim.scrollback_cursor(), Some((3, 2)));

        sim.send_attach_chord("v").expect("begin selection");
        sim.send_attach_chord("k").expect("move cursor");

        assert!(sim.selection_active());
        assert_eq!(sim.scrollback_cursor(), Some((2, 2)));
        // Selection runs from the anchor at row 3 col 2 up to row 2 col 2, so it
        // covers `     five` from col 2 and `  six` through col 2.
        assert_eq!(sim.selected_text(), Some("   five\n  s".to_string()));
    }

    #[test]
    fn attach_sim_mouse_selection_remains_owned_outside_initial_content() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_pane_lines(&["abc", "def"], 1, 1);

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 1, 1));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::Selection));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 50, 20));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::Selection));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 50, 20));

        assert!(sim.selection_active());
        assert_eq!(sim.pointer_owner(), None);
    }

    #[test]
    fn attach_sim_cancellation_policies_clear_resize_owner() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 9, 3));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::Resize));
        sim.open_help_overlay();
        assert_eq!(sim.pointer_owner(), None);

        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 9, 3));
        sim.disable_mouse();
        assert_eq!(sim.pointer_owner(), None);

        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 9, 3));
        sim.set_can_write(false);
        assert_eq!(sim.pointer_owner(), None);
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 12, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 12, 3));
        assert!(
            !sim.effects()
                .iter()
                .any(|effect| matches!(effect, AttachUiEffect::ResizePane { .. }))
        );
    }

    #[test]
    fn attach_sim_mouse_resize_emits_resize_pane_effect() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 9, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 12, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 12, 3));

        assert!(sim.effects().iter().any(|effect| {
            matches!(
                effect,
                AttachUiEffect::ResizePane {
                    direction:
                        bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right,
                    cells: 3,
                    ..
                }
            )
        }));
    }

    #[test]
    fn attach_sim_mouse_resize_retains_owner_inside_mouse_aware_pane() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.enable_pane_mouse_reporting();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 9, 3));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::Resize));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 12, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 12, 3));

        assert!(sim.effects().iter().any(|effect| {
            matches!(
                effect,
                AttachUiEffect::ResizePane {
                    direction:
                        bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right,
                    cells: 3,
                    ..
                }
            )
        }));
        assert!(sim.forwarded_mouse_bytes().is_empty());
        assert_eq!(sim.pointer_owner(), None);
    }

    #[test]
    fn attach_sim_unowned_mouse_aware_pane_event_still_forwards() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_vertical_split_panes();
        sim.enable_pane_mouse_reporting();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 3, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 4, 3));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 4, 3));

        assert_eq!(sim.pointer_owner(), None);
        assert_eq!(sim.forwarded_mouse_bytes().len(), 3);
    }

    #[test]
    fn attach_sim_mouse_floating_drag_remains_owned_across_pane_content() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_floating_pane_layout();
        sim.enable_pane_mouse_reporting();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 2, 2));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::Floating));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 15, 8));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 15, 8));

        assert!(sim.effects().iter().any(|effect| {
            matches!(effect, AttachUiEffect::MoveFloatingPane { pane_id, .. } if *pane_id == Uuid::from_u128(32))
        }));
        assert!(sim.forwarded_mouse_bytes().is_empty());
        assert_eq!(sim.pointer_owner(), None);
    }

    #[test]
    fn attach_sim_mouse_floating_drag_emits_move_effect() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_floating_pane_layout();

        sim.send_mouse(left_mouse(TerminalMousePhase::Down, 2, 2));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 6, 4));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 6, 4));

        assert!(sim.effects().iter().any(|effect| {
            matches!(
                effect,
                AttachUiEffect::MoveFloatingPane {
                    pane_id,
                    x: 6,
                    y: 4,
                } if *pane_id == Uuid::from_u128(32)
            )
        }));
    }
}
