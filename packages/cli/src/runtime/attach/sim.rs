use super::input::{TerminalGeometry, TerminalMouseEvent};
use super::runtime::{attach_tab_drop_marker_col, reduce_attach_status_tab_mouse_event};
use super::state::{AttachTabDropPlacement, AttachUiEffect, AttachViewState};
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};
use bmux_appearance::RuntimeAppearance;
use bmux_client::AttachOpenInfo;
use bmux_config::{StatusBarConfig, StatusTabOrder};
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
    effects: Vec<AttachUiEffect>,
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
            effects: Vec::new(),
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
                    row: self.geometry.rows.saturating_sub(1),
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
            row: self.geometry.rows.saturating_sub(1),
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
}
