use super::input::{TerminalGeometry, TerminalMouseEvent};
use super::runtime::{
    attach_key_event_actions, attach_tab_drop_marker_col, handle_attach_ui_action_at,
    reduce_attach_status_tab_mouse_event, status_row_for_position,
};
use super::state::{AttachTabDropPlacement, AttachUiEffect, AttachViewState, PaneRenderBuffer};
use crate::input::InputProcessor;
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};
use anyhow::{Result, bail};
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    PaneLayoutNode, PaneState, PaneSummary,
};
use bmux_attach_pipeline::render::append_pane_output;
use bmux_client::{AttachLayoutState, AttachOpenInfo};
use bmux_config::{StatusBarConfig, StatusPosition, StatusTabOrder};
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
    now: Instant,
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
            now: Instant::now(),
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
        self.render();
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
        let mut status_line = build_attach_status_line(
            self.geometry.cols,
            &self.status_config,
            &self.appearance,
            "sim",
            1,
            "sim",
            &tabs,
            None,
            "NORMAL",
            "write",
            None,
            "",
        );
        status_line.drag_marker_col = self
            .view_state
            .mouse
            .tab_drag
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

    pub fn send_mouse(&mut self, event: TerminalMouseEvent) {
        let reduction =
            reduce_attach_status_tab_mouse_event(&mut self.view_state, event, self.geometry);
        if !reduction.consumed {
            return;
        }
        for effect in reduction.effects {
            self.apply_effect(effect);
        }
        self.render();
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
                parser: vt100::Parser::new(content_height, content_width, 4_096),
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    content_width,
                    content_height,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("attach-sim pane grid dimensions are valid"),
                ..PaneRenderBuffer::default()
            });
        buffer.parser = vt100::Parser::new(content_height, content_width, 4_096);
        buffer.terminal_grid = bmux_terminal_grid::TerminalGridStream::new(
            content_width,
            content_height,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("attach-sim pane grid dimensions are valid");
        let mut bytes = lines.join("\r\n").into_bytes();
        bytes.extend_from_slice(format!("\x1b[{cursor_row};{cursor_col}H").as_bytes());
        append_pane_output(buffer, &bytes);
        self.view_state.exit_scrollback();
    }

    pub fn send_attach_chord(&mut self, chord: &str) -> Result<Vec<String>> {
        self.input_processor
            .set_scroll_mode(self.view_state.scrollback_active);
        let strokes = crate::input::parse_key_chord(chord)
            .map_err(|error| anyhow::anyhow!("invalid attach key chord '{chord}': {error}"))?;
        let mut emitted = Vec::new();
        for stroke in strokes {
            let event = crossterm_event_from_stroke(stroke);
            let CrosstermEvent::Key(key) = event else {
                continue;
            };
            for action in
                attach_key_event_actions(&key, &mut self.input_processor, self.view_state.ui_mode)?
            {
                match action {
                    super::state::AttachEventAction::Ui(ui_action) => {
                        emitted.push(format!("ui:{ui_action:?}"));
                        handle_attach_ui_action_at(&ui_action, &mut self.view_state, self.now);
                    }
                    super::state::AttachEventAction::Send(bytes) => {
                        emitted.push("send".to_string());
                        self.forwarded_bytes.push(bytes);
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
                    super::state::AttachEventAction::Mouse(_)
                    | super::state::AttachEventAction::Redraw => {}
                }
            }
        }
        let trailing = self.input_processor.process_stream_bytes(&[]);
        for ui_action in trailing {
            emitted.push(format!("ui:{ui_action:?}"));
            handle_attach_ui_action_at(&ui_action, &mut self.view_state, self.now);
        }
        self.render();
        Ok(emitted)
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

    pub const fn scrollback_active(&self) -> bool {
        self.view_state.scrollback_active
    }

    pub const fn selection_active(&self) -> bool {
        self.view_state.selection_active()
    }

    pub fn selected_text(&mut self) -> Option<String> {
        super::runtime::selected_attach_text(&mut self.view_state)
    }

    pub fn scrollback_cursor(&self) -> Option<(usize, usize)> {
        self.view_state
            .scrollback_cursor
            .map(|cursor| (cursor.row, cursor.col))
    }

    pub fn locate_text(&self, text: &str) -> Option<AttachSimLocatedText> {
        let status_line = self.view_state.cached_status_line.as_ref()?;
        for (index, window) in self.windows.iter().enumerate() {
            let indexed_label = format!("{}:{}", index + 1, window.name);
            if text == indexed_label {
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
            }
            AttachUiEffect::ShowTransientStatus { .. } => {}
        }
        self.effects.push(effect);
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
    use crate::runtime::attach::state::{AttachTabDropPlacement, AttachUiEffect};
    use bmux_config::StatusPosition;
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
    fn attach_sim_reorders_tabs_through_shared_reducer() {
        let mut sim = AttachSimHarness::new(100, 24);
        sim.seed_window_list(&["one", "two", "three"], "one");
        assert!(sim.rendered().contains("1:one"));

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
        assert!(sim.rendered().contains("1:two"));
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
        assert_eq!(sim.selected_text(), Some("e\n  f".to_string()));
    }
}
