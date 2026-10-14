//! Presenter-owned controls. Geometry and clipping belong exclusively to the TUI scene.
use crate::playbook_controller::{Control, Controller, LifecycleSnapshot};
use bmux_tui::{
    component::LogicalSize,
    style::{Color, Modifier, Style},
};
use bmux_tui::{
    embedded_surface::RetainedSurface,
    prelude::{
        Column, Component, ComponentRevision, Constraints, Element, Event, EventCx, EventOutcome,
        LayoutCx, LayoutNode, PaintCx, Row, SizeBox, Surface, TextBlock,
    },
};
use bmux_tui_components::button::{ButtonComponent, ButtonState, ButtonStyles};
use std::{
    cell::Cell,
    hash::{Hash, Hasher},
};

pub(crate) const INPUTS: [&str; 13] = [
    "play-toggle",
    "step",
    "stop",
    "restart",
    "close",
    "speed:half",
    "speed:normal",
    "speed:double",
    "speed:instant",
    "pan:left",
    "pan:right",
    "pan:up",
    "pan:down",
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
    buttons: [Cell<ButtonState>; 13],
    viewport: (u16, u16),
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
            let inactive = terminal && index < 2
                || index == 2
                    && snapshot
                        .attempt
                        .as_ref()
                        .is_none_or(|attempt| attempt.cleanup_acknowledged);
            state.set_disabled(index != 4 && (snapshot.unresolved || inactive));
            Cell::new(state)
        });
        Self {
            title,
            controller,
            snapshot,
            audience: false,
            surface: None,
            buttons,
            viewport: (0, 0),
        }
    }
    pub fn viewport(mut self, viewport: (u16, u16)) -> Self {
        self.viewport = viewport;
        self
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
        let speed = self
            .snapshot
            .attempt
            .as_ref()
            .map_or("normal", |a| a.playback_speed.as_str());
        let selected = INPUTS[index].strip_prefix("speed:") == Some(speed);
        let normal = Style::new().fg(Color::White).bg(Color::BrightBlack);
        let active = Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        ButtonComponent::new(
            format!("playbook-control/{}", INPUTS[index]),
            label,
            &self.buttons[index],
        )
        .filled()
        .styles(ButtonStyles {
            normal: if selected {
                active.add_modifier(Modifier::UNDERLINE)
            } else {
                normal
            },
            focused: active.add_modifier(Modifier::UNDERLINE),
            hovered: active,
            pressed: active.add_modifier(Modifier::REVERSED),
            disabled: normal.add_modifier(Modifier::DIM),
        })
    }
    fn toolbar(&self, width: u64) -> Element<'_> {
        let play = if play_control(self.snapshot) == Control::Pause {
            "Ⅱ Pause"
        } else {
            "▶ Play"
        };
        let mut column = Column::new();
        if !self.audience {
            if width >= 66 {
                column = column
                    .child(
                        Row::new()
                            .child(self.button(0, play))
                            .child(self.button(1, "▸ Step"))
                            .child(self.button(2, "■ Stop"))
                            .child(self.button(3, "↻ Restart"))
                            .child(self.button(4, "× Close")),
                    )
                    .child(
                        Row::new()
                            .child(self.button(5, "Half 0.5×"))
                            .child(self.button(6, "Normal 1×"))
                            .child(self.button(7, "Double 2×"))
                            .child(self.button(8, "Instant")),
                    );
            } else {
                column = column.child(self.button(4, "× Close"));
                for (i, label) in [
                    (0, play),
                    (1, "▸ Step"),
                    (2, "■ Stop"),
                    (3, "↻ Restart"),
                    (5, "Half 0.5×"),
                    (6, "Normal 1×"),
                    (7, "Double 2×"),
                    (8, "Instant"),
                ] {
                    column = column.child(self.button(i, label));
                }
            }
        }
        Element::new(column)
    }
    fn header(&self) -> Element<'_> {
        let status = self.snapshot.attempt.as_ref().map_or_else(
            || {
                self.snapshot
                    .acknowledgment
                    .clone()
                    .unwrap_or_else(|| "Not running · speed normal".into())
            },
            |a| {
                if a.phase == "script-complete" {
                    format!(
                        "Complete · {}/{} actions · session retained · {}",
                        a.step, a.action_count, a.playback_speed
                    )
                } else {
                    format!(
                        "{}{} · action {}/{} · {} · {}",
                        a.phase,
                        if a.paused { " · paused" } else { "" },
                        a.step,
                        a.action_count,
                        a.playback_speed,
                        a.current_action
                    )
                }
            },
        );
        let mut column = Column::new()
            .child(TextBlock::new(self.title))
            .child(TextBlock::new(bounded_diagnostic(&status)));
        if !self.audience
            && let Some(error) = &self.snapshot.error
        {
            column = column.child(TextBlock::new(bounded_diagnostic(error)));
        }
        Element::new(column)
    }
    fn tree(&self, width: u64, height: u64) -> Element<'_> {
        let inner = width.saturating_sub(2);
        let measure = |element: Element<'_>| {
            element
                .layout(Constraints::for_width(inner), &mut LayoutCx::default())
                .size
                .height
        };
        let chrome = measure(self.header()) + measure(self.toolbar(inner)) + 2;
        let mut column = Column::new().child(self.header());
        if let Some(surface) = self.surface {
            let area = surface.buffer().area();
            let clipped = u64::from(area.width) > inner
                || u64::from(area.height) > height.saturating_sub(chrome);
            let pan_height = if clipped {
                if self.audience { 1 } else { 2 }
            } else {
                0
            };
            let available = height.saturating_sub(chrome + pan_height);
            column = column.child(
                SizeBox::new(PlayerViewport {
                    surface,
                    offset: self.viewport,
                })
                .height(available),
            );
            if clipped {
                column = column.child(TextBlock::new(format!(
                    "Viewport cropped · source {}×{} · pan to see edges",
                    area.width, area.height
                )));
                if !self.audience {
                    column = column.child(
                        Row::new()
                            .child(self.button(9, "←"))
                            .child(self.button(10, "→"))
                            .child(self.button(11, "↑"))
                            .child(self.button(12, "↓")),
                    );
                }
            }
        }
        column = column.child(self.toolbar(inner));
        Element::new(
            SizeBox::new(Surface::new(column).border(bmux_tui::chrome::Border::new(
                bmux_tui::chrome::BorderSet::ROUNDED,
                Style::new(),
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
        self.viewport.hash(&mut state);
        ComponentRevision::new(state.finish(), state.finish())
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let area = self.surface.map(|s| s.buffer().area());
        let width = constraints
            .max_width()
            .min(area.map_or(88, |a| u64::from(a.width) + 2).max(70));
        let chrome = self
            .header()
            .layout(Constraints::for_width(width.saturating_sub(2)), cx)
            .size
            .height
            + self
                .toolbar(width.saturating_sub(2))
                .layout(Constraints::for_width(width.saturating_sub(2)), cx)
                .size
                .height
            + 2;
        let height = constraints
            .max_height()
            .unwrap_or(128)
            .min(area.map_or(8, |a| u64::from(a.height)) + chrome);
        self.tree(width, height)
            .layout(Constraints::new(0, width, 0, Some(height)), cx)
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

/// Presenter-local view transform. Never resizes or sends input to execution.
struct PlayerViewport<'a> {
    surface: &'a RetainedSurface,
    offset: (u16, u16),
}
impl Component for PlayerViewport<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        let area = self.surface.buffer().area();
        LayoutNode::leaf(
            "playbook-viewport".into(),
            constraints.constrain(LogicalSize::new(
                u64::from(area.width),
                u64::from(area.height),
            )),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let mut placement = self.surface.placement(width, height);
        placement.source_x = self.offset.0;
        placement.source_y = self.offset.1;
        self.surface.paint_placement(cx, width, height, placement);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_viewport_keeps_all_corners_and_small_viewport_can_pan() {
        use bmux_tui::{
            buffer::Buffer,
            embedded_surface::{SurfaceIdentity, SurfaceLimits},
            geometry::{Rect, Size},
            image_scene::ImageScene,
        };
        use std::sync::Arc;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
        for (x, y, symbol) in [(0, 0, "A"), (79, 0, "B"), (0, 23, "C"), (79, 23, "D")] {
            buffer.set_cell(bmux_tui::geometry::Point::new(x, y), symbol, Style::new());
        }
        let retained = RetainedSurface::new(
            SurfaceIdentity {
                owner: "test".into(),
                generation: 1,
                revision: 1,
            },
            Arc::new(buffer),
            Arc::new(ImageScene::default()),
            SurfaceLimits {
                max_cells: 10000,
                max_bytes: 1024 * 1024,
            },
        )
        .unwrap();
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
        for (width, height, offset, expected) in [
            (100, 40, (0, 0), "ABCD"),
            (60, 24, (0, 0), "A"),
            (60, 24, (u16::MAX, u16::MAX), "D"),
        ] {
            let demo = PlaybookDemo::new("Demo", &controller, &snapshot)
                .retained_surface(&retained)
                .viewport(offset);
            let layout = demo.layout(
                Constraints::loose(Size::new(width, height)),
                &mut LayoutCx::default(),
            );
            let mut output = Buffer::empty(Rect::new(0, 0, width, height));
            let mut frame = bmux_tui::frame::Frame::new(&mut output);
            demo.paint(&layout, &mut PaintCx::new(&mut frame));
            let text = output
                .cells()
                .iter()
                .map(|c| c.symbol.as_str())
                .collect::<String>();
            for corner in expected.chars() {
                assert!(text.contains(corner), "missing corner {corner}");
            }
            assert_eq!(text.contains("Viewport cropped"), width < 82);
            assert!(!text.contains("[ Pause ]"));
        }
    }
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
