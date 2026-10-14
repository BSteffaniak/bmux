//! Presenter-owned controls. Geometry and clipping belong exclusively to the TUI scene.
use crate::playbook_controller::{Control, Controller, LifecycleSnapshot};
use bmux_tui::{
    embedded_surface::{EmbeddedSurface, RetainedSurface},
    prelude::{
        Column, Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome,
        LayoutCx, LayoutNode, PaintCx, Row, SizeBox, Surface, TextBlock,
    },
};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use std::{
    cell::Cell,
    hash::{Hash, Hasher},
};

pub(crate) const INPUTS: [&str; 9] = [
    "play-toggle",
    "step",
    "stop",
    "restart",
    "close",
    "speed:half",
    "speed:normal",
    "speed:double",
    "speed:instant",
];

pub(crate) fn target_input(id: &str) -> Option<&'static str> {
    let input = id.strip_prefix("playbook-control/")?;
    INPUTS.into_iter().find(|candidate| *candidate == input)
}

pub(crate) fn play_control(snapshot: &LifecycleSnapshot) -> Control {
    snapshot.attempt.as_ref().map_or(Control::Run, |attempt| {
        if attempt.phase == "prepared" {
            Control::Run
        } else if attempt.paused {
            Control::Resume
        } else {
            Control::Pause
        }
    })
}

fn bounded_diagnostic(source: &str) -> String {
    let mut diagnostic = source
        .chars()
        .take(2048)
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .collect::<String>();
    if source.chars().nth(2048).is_some() {
        diagnostic.push_str("\n[diagnostic truncated]");
    }
    diagnostic
}

pub struct PlaybookDemo<'a> {
    title: &'a str,
    controller: &'a Controller,
    snapshot: &'a LifecycleSnapshot,
    audience: bool,
    surface: Option<&'a RetainedSurface>,
    buttons: [Cell<ButtonState>; 9],
}
impl<'a> PlaybookDemo<'a> {
    pub fn new(
        title: &'a str,
        controller: &'a Controller,
        snapshot: &'a LifecycleSnapshot,
    ) -> Self {
        let terminal = snapshot.attempt.as_ref().is_some_and(|attempt| {
            !matches!(attempt.phase.as_str(), "prepared" | "running") && !attempt.paused
        });
        let buttons = std::array::from_fn(|index| {
            let mut state = ButtonState::new();
            state.set_disabled(index != 4 && (snapshot.unresolved || terminal && index < 3));
            Cell::new(state)
        });
        Self {
            title,
            controller,
            snapshot,
            audience: false,
            surface: None,
            buttons,
        }
    }
    pub fn audience(mut self, audience: bool) -> Self {
        self.audience = audience;
        self
    }
    pub fn retained_surface(mut self, surface: &'a RetainedSurface) -> Self {
        self.surface = Some(surface);
        self
    }
    pub fn submit(&self, control: Control) -> Result<(), String> {
        if self.audience {
            return Err("audience view cannot admit playbook controls".into());
        }
        if self.snapshot.owner_epoch != self.controller.owner_epoch() {
            return Err("stale playbook controller epoch".into());
        }
        self.controller.admit(
            self.snapshot
                .attempt
                .as_ref()
                .map(|attempt| attempt.generation),
            control,
        )
    }
    fn button(&self, index: usize, label: &'static str) -> ButtonComponent<'_, '_> {
        ButtonComponent::new(
            format!("playbook-control/{}", INPUTS[index]),
            label,
            &self.buttons[index],
        )
    }
    fn tree(&self, width: u64, height: u64) -> Element<'_> {
        let status = self.snapshot.attempt.as_ref().map_or_else(
            || {
                self.snapshot.acknowledgment.as_ref().map_or_else(
                    || "Not running · speed normal".into(),
                    |acknowledgment| bounded_diagnostic(acknowledgment),
                )
            },
            |attempt| {
                format!(
                    "{}{} · action {}/{} · {}\n{}",
                    attempt.phase,
                    if attempt.paused { " · paused" } else { "" },
                    attempt.step,
                    attempt.action_count,
                    attempt.playback_speed,
                    bounded_diagnostic(&attempt.current_action)
                )
            },
        );
        let mut column = Column::new().child(TextBlock::new(self.title));
        if !self.audience {
            let play = if play_control(self.snapshot) == Control::Pause {
                "Pause"
            } else {
                "Play"
            };
            if width >= 58 {
                column = column
                    .child(
                        Row::new()
                            .child(self.button(0, play))
                            .child(self.button(1, "Step"))
                            .child(self.button(2, "Stop"))
                            .child(self.button(3, "Restart"))
                            .child(self.button(4, "Close")),
                    )
                    .child(
                        Row::new()
                            .child(self.button(5, "Half"))
                            .child(self.button(6, "Normal"))
                            .child(self.button(7, "Double"))
                            .child(self.button(8, "Instant")),
                    );
            } else {
                // Genuine button leaves, never a textual approximation. Close remains first
                // in the tiny fallback; clipped controls have no hit regions.
                column = column.child(self.button(4, "Close"));
                for (index, label) in [
                    (0, play),
                    (1, "Step"),
                    (2, "Stop"),
                    (3, "Restart"),
                    (5, "Half"),
                    (6, "Normal"),
                    (7, "Double"),
                    (8, "Instant"),
                ] {
                    column = column.child(self.button(index, label));
                }
            }
        }
        column = column.child(TextBlock::new(status));
        if let Some(surface) = self.surface {
            let controls_height = if self.audience {
                5
            } else if width >= 58 {
                7
            } else {
                14
            };
            column = column.child(
                SizeBox::new(EmbeddedSurface::new(surface))
                    .height(height.saturating_sub(controls_height)),
            );
        }
        if !self.audience
            && let Some(error) = &self.snapshot.error
        {
            column = column.child(TextBlock::new(bounded_diagnostic(error)));
        }
        Element::new(
            SizeBox::new(Surface::new(column).border(bmux_tui::chrome::Border::new(
                bmux_tui::chrome::BorderSet::ROUNDED,
                bmux_tui::style::Style::new(),
            )))
            .width(width)
            .height(height),
        )
    }
}
impl Component for PlaybookDemo<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut state = std::collections::hash_map::DefaultHasher::new();
        self.snapshot.owner_epoch.hash(&mut state);
        self.snapshot.revision.hash(&mut state);
        self.audience.hash(&mut state);
        self.title.hash(&mut state);
        ComponentRevision::new(state.finish(), state.finish())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree(
            constraints.max_width().saturating_sub(4).clamp(1, 88),
            constraints
                .max_height()
                .unwrap_or(28)
                .saturating_sub(2)
                .clamp(1, 28),
        )
        .layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree(layout.size.width, layout.size.height)
            .paint(layout, cx);
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        // Owner routes a single committed target through the same admission/auth boundary.
        EventOutcome::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn speed_targets_and_close_are_exact_not_generic_clicks() {
        for input in INPUTS {
            assert_eq!(
                target_input(&format!("playbook-control/{input}")),
                Some(input)
            );
        }
        assert_eq!(target_input("background"), None);
        assert_eq!(target_input("playbook-control/seek:1"), None);
    }
    #[test]
    fn tiny_modal_clips_buttons_and_uses_only_scene_targets() {
        let snapshot = LifecycleSnapshot {
            owner_epoch: uuid::Uuid::new_v4(),
            revision: 0,
            attempt: None,
            acknowledgment: None,
            surface: None,
            cancellation_revision: 0,
            unresolved: false,
            error: None,
        };
        let controller = Controller::detached(snapshot.clone());
        for (width, height) in [(80, 24), (24, 16), (3, 2)] {
            let scene = bmux_tui::presentation::PresentationState::new(
                bmux_tui::geometry::Rect::new(0, 0, width, height),
            );
            let modal = bmux_tui::composition::Overlay::new(
                TextBlock::new("background"),
                PlaybookDemo::new("Demo", &controller, &snapshot),
            )
            .modal(bmux_tui::focus::FocusScopeId::new("demo"));
            let layout = modal.layout(
                Constraints::tight(bmux_tui::geometry::Size::new(width, height)),
                &mut LayoutCx::default(),
            );
            let mut prepared = scene
                .prepare(bmux_tui::damage::Damage::Full, |cx| {
                    modal.paint(&layout, cx)
                })
                .unwrap();
            prepared.set_modal_scopes(&[bmux_tui::focus::FocusScopeId::new("demo")]);
            for target in prepared.hits().regions() {
                assert!(target.area.x.saturating_add(target.area.width) <= width);
                assert!(target.area.y.saturating_add(target.area.height) <= height);
                if target_input(target.id.as_str()).is_some() {
                    assert!(target.focus_scope.is_some());
                }
            }
            if width == 80 {
                assert_eq!(
                    prepared
                        .hits()
                        .regions()
                        .iter()
                        .filter(|hit| target_input(hit.id.as_str()).is_some())
                        .count(),
                    9
                );
            }
        }
    }

    #[test]
    fn play_toggle_follows_authoritative_pause_state() {
        let fixture_snapshot = LifecycleSnapshot {
            owner_epoch: uuid::Uuid::new_v4(),
            revision: 0,
            attempt: None,
            acknowledgment: None,
            surface: None,
            cancellation_revision: 0,
            unresolved: false,
            error: None,
        };
        assert_eq!(play_control(&fixture_snapshot), Control::Run);
    }

    #[test]
    fn diagnostics_are_bounded_utf8_and_not_terminal_commands() {
        let bounded = bounded_diagnostic(&format!("\u{1b}[31m{}", "é".repeat(4096)));
        assert!(!bounded.contains('\u{1b}'));
        assert!(bounded.ends_with("[diagnostic truncated]"));
        assert!(bounded.len() < 4200);
    }
}
