use super::adapters::{AttachClock, FixedAttachClock};
use super::input::{TerminalGeometry, TerminalMouseEvent};
#[cfg(test)]
use super::prompt_ui::AttachInternalPromptAction;
use super::prompt_ui::PromptKeyDisposition;
use super::runtime::{
    AttachPointerContinuation, attach_key_event_actions, attach_mouse_forward_bytes_for_target,
    attach_tab_drop_marker_col, build_attach_help_lines, continue_attach_builtin_pointer_owner,
    encode_bracketed_paste, focused_attach_pane_input_mode, handle_attach_ui_action_at,
    handle_help_overlay_key_event, maybe_begin_attach_mouse_selection_drag,
    reduce_attach_mouse_floating_drag_event, reduce_attach_mouse_resize_event,
    reduce_attach_status_tab_mouse_event, status_row_for_position,
};
use super::state::{
    AttachPointerOwner, AttachTabDropPlacement, AttachUiEffect, AttachViewState, PaneRenderBuffer,
};
use crate::input::{InputProcessor, RuntimeAction};
#[cfg(test)]
use crate::runtime::prompt::PromptRequest;
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};
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
    #[cfg(test)]
    pub fn tab_rename_cursor_col(&self) -> Option<u16> {
        self.view_state
            .cached_status_line
            .as_ref()
            .and_then(|line| line.edit_cursor_col)
    }

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

    #[cfg(test)]
    pub fn send_rename_paste(&mut self, text: &str) {
        if let Some(reduction) =
            super::runtime::handle_attach_tab_rename_paste(&mut self.view_state, text)
        {
            for effect in reduction.effects {
                self.apply_effect(effect);
            }
            self.render();
        }
    }

    #[cfg(test)]
    pub fn set_mru_tab_order(&mut self) {
        self.set_tab_order(StatusTabOrder::Mru);
    }

    pub fn render(&mut self) -> &AttachStatusLine {
        let tabs = self
            .windows
            .iter()
            .map(|window| AttachTab {
                label: window.name.clone(),
                active: window.active,
                context_id: Some(window.id),
            })
            .collect::<Vec<_>>();
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
        let mut status_line = build_attach_status_line(
            self.geometry.cols,
            &self.status_config,
            &self.appearance,
            "sim",
            1,
            "sim",
            &crate::status::AttachTabStripInput::new(&tabs)
                .hovered(self.view_state.mouse.hovered_tab_context_id)
                .editing(self.view_state.tab_rename.as_ref().map(|rename| {
                    let selection = rename.buffer.selection();
                    crate::status::AttachTabEdit {
                        context_id: rename.context_id,
                        text: rename.text(),
                        cursor: rename.buffer.cursor_byte_index(),
                        selection: selection.map(|range| (range.start, range.end)),
                    }
                })),
            None,
            mode_label,
            "write",
            None,
            hint,
        );
        status_line.drag_marker_col = self
            .view_state
            .mouse
            .tab_drag
            // Mirror production: the marker appears only for an active drag.
            .filter(|drag| drag.active)
            .and_then(|drag| drag.drop_target)
            .and_then(|target| {
                attach_tab_drop_marker_col(&status_line, target, self.geometry.cols)
            });
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
    use crate::runtime::attach::state::{
        AttachPointerOwner, AttachTabDropPlacement, AttachUiEffect,
    };
    use bmux_attach_layout_protocol::AttachFocusTarget;
    use bmux_config::StatusPosition;
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

    #[test]
    fn attach_sim_reorders_tabs_through_shared_reducer() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");
        assert!(sim.rendered().contains("one"));

        let one = sim.locate_text("1:one").expect("one tab");
        let three = sim.locate_text("3:three").expect("three tab");
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            one.center_col,
            one.row,
        ));
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Move,
            three.end_col,
            three.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, three.end_col, three.row));

        assert_eq!(
            sim.effects(),
            &[AttachUiEffect::MoveWindow {
                source_context_id: Uuid::from_u128(1),
                target_context_id: Uuid::from_u128(3),
                placement: AttachTabDropPlacement::After,
            }]
        );
        assert_eq!(sim.window_names(), ["two", "three", "one"]);
        // Indexes renumber after a reorder; assert that against an explicitly
        // indexed template so the check survives tab-template default changes.
        sim.set_tab_template("{index}:{name}");
        assert!(sim.rendered().contains("1:two"));
        assert!(sim.rendered().contains("3:one"));
    }

    #[test]
    fn attach_sim_status_position_changes_located_mouse_row() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");
        let bottom = sim.locate_text("1:one").expect("bottom tab");
        assert_eq!(bottom.row, 23);

        sim.set_status_position(StatusPosition::Top);
        let top = sim.locate_text("1:one").expect("top tab");
        assert_eq!(top.row, 0);
    }

    fn key(code: super::super::input::TerminalKeyCode) -> super::super::input::TerminalKeyEvent {
        super::super::input::TerminalKeyEvent {
            code,
            modifiers: TerminalModifiers::default(),
            kind: super::super::input::TerminalKeyPhase::Press,
        }
    }

    fn double_click_tab(sim: &mut AttachSimHarness, name: &str) {
        let tab = sim.locate_text(name).expect("tab should be located");
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, tab.center_col, tab.row));
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));
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

    fn open_tab_menu(sim: &mut AttachSimHarness, name: &str) -> super::AttachSimLocatedText {
        let tab = sim.locate_text(name).expect("tab should be located");
        sim.send_mouse(right_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));
        tab
    }

    #[test]
    fn attach_sim_right_click_opens_tab_menu_without_switching() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "two");

        assert!(sim.tab_menu_active(), "right-click should open the menu");
        assert_eq!(
            sim.active_window_name(),
            Some("one"),
            "opening the menu must not switch windows"
        );
        assert_eq!(
            sim.render().drag_marker_col,
            None,
            "no drag should be armed"
        );
    }

    #[test]
    fn attach_sim_tab_menu_disables_position_moves_at_the_edges() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "one");
        let items = sim.tab_menu_items();
        assert!(
            items.contains(&"move-left:disabled".to_string()),
            "first tab cannot move left: {items:?}"
        );
        assert!(
            items.contains(&"move-to-first:disabled".to_string()),
            "{items:?}"
        );
        assert!(items.contains(&"move-right".to_string()), "{items:?}");

        sim.send_menu_chord("Esc");
        open_tab_menu(&mut sim, "three");
        let items = sim.tab_menu_items();
        assert!(
            items.contains(&"move-right:disabled".to_string()),
            "last tab cannot move right: {items:?}"
        );
        assert!(items.contains(&"move-left".to_string()), "{items:?}");
    }

    #[test]
    fn attach_sim_tab_menu_focus_skips_disabled_items_and_wraps() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        open_tab_menu(&mut sim, "one");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("rename"));

        sim.send_menu_chord("Down");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("close"));
        // move-left / move-to-first are disabled on the first tab.
        sim.send_menu_chord("Down");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("move-right"));

        // Wrapping upward returns to the first enabled entry.
        sim.send_menu_chord("Up");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("close"));

        sim.send_menu_chord("End");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("new-window"));
        sim.send_menu_chord("Home");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("rename"));
    }

    #[test]
    fn attach_sim_tab_menu_escape_dismisses() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        open_tab_menu(&mut sim, "two");
        assert!(sim.tab_menu_active());
        sim.send_menu_chord("Esc");
        assert!(!sim.tab_menu_active());
    }

    #[test]
    fn attach_sim_tab_menu_rename_opens_inline_editor_for_clicked_tab() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "three");
        sim.send_menu_chord("Enter");

        assert!(!sim.tab_menu_active());
        assert!(sim.tab_rename_active(), "rename should open the editor");
        assert_eq!(
            sim.tab_rename_text(),
            Some("three"),
            "editor should target the right-clicked tab"
        );
    }

    #[test]
    fn attach_sim_tab_menu_close_targets_the_clicked_tab() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "three");
        sim.send_menu_chord("Down");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("close"));
        sim.send_menu_chord("Enter");

        let closed = sim
            .effects()
            .iter()
            .find_map(|effect| match effect {
                AttachUiEffect::CloseWindow { context_id } => Some(*context_id),
                _ => None,
            })
            .expect("close should emit a CloseWindow effect");
        assert_eq!(
            closed,
            Uuid::from_u128(3),
            "close must target the clicked tab, not the active one"
        );
    }

    #[test]
    fn attach_sim_tab_menu_move_right_emits_move_after_neighbor() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "one");
        // rename -> close -> move-right (move-left disabled on first tab)
        sim.send_menu_chord("Down");
        sim.send_menu_chord("Down");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("move-right"));
        sim.send_menu_chord("Enter");

        assert_eq!(sim.window_names(), ["two", "one", "three"]);
    }

    #[test]
    fn attach_sim_tab_menu_move_to_last_moves_to_the_end() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        open_tab_menu(&mut sim, "one");
        sim.send_menu_chord("End");
        sim.send_menu_chord("Up");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("move-to-last"));
        sim.send_menu_chord("Enter");

        assert_eq!(sim.window_names(), ["two", "three", "one"]);
    }

    #[test]
    fn attach_sim_tab_menu_new_window_appends_and_activates() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        open_tab_menu(&mut sim, "one");
        sim.send_menu_chord("End");
        assert_eq!(sim.tab_menu_focused().as_deref(), Some("new-window"));
        sim.send_menu_chord("Enter");

        assert_eq!(sim.window_names().len(), 3);
        assert_eq!(sim.active_window_name(), Some("tab-3"));
    }

    #[test]
    fn attach_sim_tab_menu_click_outside_dismisses() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        let tab = open_tab_menu(&mut sim, "two");
        assert!(sim.tab_menu_active());
        // Click far from the menu overlay.
        sim.send_mouse(left_mouse(TerminalMousePhase::Down, tab.center_col, 2));
        assert!(!sim.tab_menu_active());
    }

    #[test]
    fn attach_sim_tab_menu_ignores_disabled_item_activation() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one"], "one");

        open_tab_menu(&mut sim, "one");
        // Only rename, close, and new-window are enabled for a lone tab.
        let items = sim.tab_menu_items();
        assert!(
            items.contains(&"move-left:disabled".to_string()),
            "{items:?}"
        );
        assert!(
            items.contains(&"move-right:disabled".to_string()),
            "{items:?}"
        );

        // Focus can never land on a disabled entry, so Enter cannot activate it.
        for _ in 0..items.len() {
            sim.send_menu_chord("Down");
            let focused = sim.tab_menu_focused();
            assert!(
                matches!(focused.as_deref(), Some("rename" | "close" | "new-window")),
                "focus landed on a disabled entry: {focused:?}"
            );
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
    fn attach_sim_double_click_opens_inline_rename_with_name_selected() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        double_click_tab(&mut sim, "two");

        assert!(
            sim.tab_rename_active(),
            "double-click should open the editor"
        );
        assert_eq!(sim.tab_rename_text(), Some("two"));
        // Whole name selected: typing replaces it outright.
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char('x')));
        assert_eq!(sim.tab_rename_text(), Some("x"));
    }

    #[test]
    fn attach_sim_single_click_does_not_open_rename() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");
        let tab = sim.locate_text("two").expect("tab");

        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, tab.center_col, tab.row));

        assert!(!sim.tab_rename_active());
    }

    #[test]
    fn attach_sim_slow_second_click_does_not_open_rename() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");
        let tab = sim.locate_text("two").expect("tab");

        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, tab.center_col, tab.row));
        // Past the double-click window.
        sim.advance_clock(std::time::Duration::from_secs(1));
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            tab.center_col,
            tab.row,
        ));

        assert!(!sim.tab_rename_active());
    }

    #[test]
    fn attach_sim_rename_commit_renames_window_and_restores_template() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");
        sim.set_tab_template("[{name}]");

        double_click_tab(&mut sim, "two");
        // Raw editor text replaces the template while editing.
        assert!(sim.rendered().contains("two"), "{:?}", sim.rendered());

        for ch in "dev".chars() {
            sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char(ch)));
        }
        assert_eq!(sim.tab_rename_text(), Some("dev"));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Enter));

        assert!(!sim.tab_rename_active());
        assert_eq!(sim.window_names(), ["one", "dev", "three"]);
        // Template chrome is restored after commit.
        assert!(sim.rendered().contains("[dev]"), "{:?}", sim.rendered());
    }

    #[test]
    fn attach_sim_rename_escape_restores_original_name() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        double_click_tab(&mut sim, "two");
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char('z')));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Esc));

        assert!(!sim.tab_rename_active());
        assert_eq!(sim.window_names(), ["one", "two"]);
        assert!(
            !sim.effects()
                .iter()
                .any(|effect| matches!(effect, AttachUiEffect::RenameWindow { .. })),
            "escape must not emit a rename"
        );
    }

    #[test]
    fn attach_sim_rename_arrow_key_appends_instead_of_replacing() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        double_click_tab(&mut sim, "two");
        // Right arrow collapses the selection to the end, so typing appends.
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Right));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char('!')));

        assert_eq!(sim.tab_rename_text(), Some("two!"));
    }

    #[test]
    fn attach_sim_rename_backspace_and_home_behave_like_a_text_input() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "alpha"], "one");

        double_click_tab(&mut sim, "alpha");
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::End));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Backspace));
        assert_eq!(sim.tab_rename_text(), Some("alph"));

        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Home));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char('_')));
        assert_eq!(sim.tab_rename_text(), Some("_alph"));
    }

    #[test]
    fn attach_sim_rename_blank_name_is_not_committed() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        double_click_tab(&mut sim, "two");
        // Selection covers the whole name, so a space replaces it entirely.
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Char(' ')));
        sim.send_rename_key(&key(super::super::input::TerminalKeyCode::Enter));

        assert!(!sim.tab_rename_active());
        assert_eq!(
            sim.window_names(),
            ["one", "two"],
            "blank names must be rejected"
        );
    }

    #[test]
    fn attach_sim_rename_paste_collapses_newlines() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        double_click_tab(&mut sim, "two");
        sim.send_rename_paste("multi\nline");

        assert_eq!(sim.tab_rename_text(), Some("multi line"));
    }

    #[test]
    fn attach_sim_rename_cursor_column_is_reported_for_rendering() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");

        double_click_tab(&mut sim, "two");
        let cursor = sim
            .tab_rename_cursor_col()
            .expect("editor should expose a cursor column");
        let tab = sim.locate_text("two").expect("tab");
        assert!(
            cursor >= tab.start_col && cursor <= tab.end_col.saturating_add(1),
            "cursor {cursor} should sit within the edited tab {}..={}",
            tab.start_col,
            tab.end_col
        );
    }

    #[test]
    fn attach_sim_double_click_does_not_start_a_tab_drag() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");

        double_click_tab(&mut sim, "two");
        assert_eq!(sim.render().drag_marker_col, None);

        // Moving after the double-click edits text, it must not reorder tabs.
        let three = sim.locate_text("three").expect("three tab");
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Move,
            three.end_col,
            three.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, three.end_col, three.row));
        assert_eq!(sim.window_names(), ["one", "two", "three"]);
    }

    #[test]
    fn attach_sim_drag_marker_only_appears_once_drag_is_active() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");
        let one = sim.locate_text("1:one").expect("one tab");
        let three = sim.locate_text("3:three").expect("three tab");

        // Plain mouse-down must not paint a drop marker.
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            one.center_col,
            one.row,
        ));
        assert_eq!(
            sim.render().drag_marker_col,
            None,
            "mouse-down alone should not show a drag marker"
        );

        // Motion past the threshold starts the drag and shows the marker.
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Move,
            three.end_col,
            three.row,
        ));
        assert!(
            sim.render().drag_marker_col.is_some(),
            "active drag should show a drop marker"
        );

        // Releasing clears it again.
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, three.end_col, three.row));
        assert_eq!(sim.render().drag_marker_col, None);
    }

    #[test]
    fn attach_sim_mru_order_does_not_move() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");
        sim.set_mru_tab_order();

        let one = sim.locate_text("1:one").expect("one tab");
        let three = sim.locate_text("3:three").expect("three tab");
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            one.center_col,
            one.row,
        ));
        sim.send_mouse(left_mouse(
            TerminalMousePhase::Move,
            three.end_col,
            three.row,
        ));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, three.end_col, three.row));

        assert_eq!(sim.window_names(), ["one", "two", "three"]);
        assert!(
            !sim.effects()
                .iter()
                .any(|effect| matches!(effect, AttachUiEffect::MoveWindow { .. }))
        );
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
    fn attach_sim_status_tab_drag_remains_owned_outside_status_row() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two"], "one");
        sim.seed_vertical_split_panes();
        sim.enable_pane_mouse_reporting();
        let one = sim.locate_text("1:one").expect("one tab");

        sim.send_mouse(left_mouse(
            TerminalMousePhase::Down,
            one.center_col,
            one.row,
        ));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::StatusTab));
        sim.send_mouse(left_mouse(TerminalMousePhase::Drag, 3, 3));
        assert_eq!(sim.pointer_owner(), Some(AttachPointerOwner::StatusTab));
        sim.send_mouse(left_mouse(TerminalMousePhase::Up, 3, 3));

        assert!(sim.forwarded_mouse_bytes().is_empty());
        assert_eq!(sim.pointer_owner(), None);
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
