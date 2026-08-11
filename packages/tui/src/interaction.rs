//! Automatic routing over the last successfully committed interaction scene.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::event::{Event, MouseButton, MouseEventKind};
use crate::focus::{FocusId, FocusKeyOutcome, FocusScopeId, FocusTrap};
use crate::hit::{HitId, HitMap};

/// Result of routing one terminal event through committed interaction metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionRoute {
    /// Original event supplied by the terminal backend.
    pub event: Event,
    /// Target resolved for keyboard or pointer dispatch.
    pub target: Option<HitId>,
    /// Previously active hover target, when this event changed it.
    pub hover_left: Option<HitId>,
    /// Newly active hover target, when this event changed it.
    pub hover_entered: Option<HitId>,
    /// Newly active keyboard focus, when this event changed it.
    pub focus_changed: Option<FocusId>,
    /// Whether the event is consumed by generic traversal rather than a control.
    pub traversal_consumed: bool,
}

impl InteractionRoute {
    /// Whether this event represents semantic control activation.
    #[must_use]
    pub const fn is_activation(&self) -> bool {
        matches!(
            self.event,
            Event::Key(KeyStroke {
                key: KeyCode::Enter | KeyCode::Space | KeyCode::Char(' '),
                ..
            }) | Event::Mouse(crate::event::MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..
            })
        )
    }

    /// Return whether transient interaction state changed.
    #[must_use]
    pub const fn needs_redraw(&self) -> bool {
        self.hover_left.is_some() || self.hover_entered.is_some() || self.focus_changed.is_some()
    }
}

/// Stateful router for pointer and keyboard interaction.
///
/// The scene is replaced only from committed presentation metadata. Pointer
/// lookup is layered and focus traversal is precomputed at scene commit time,
/// making normal event routing linear only in the small ordered focus list and
/// hit lookup in the existing scene representation.
#[derive(Debug, Clone, Default)]
pub struct InteractionRouter {
    scene: HitMap,
    focus: FocusTrap,
    focus_scope: Option<FocusScopeId>,
    restore_focus: Option<FocusId>,
    hovered: Option<HitId>,
    pressed: Option<HitId>,
}

impl InteractionRouter {
    /// Create an empty router.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scene: HitMap::new(),
            focus: FocusTrap::new(),
            focus_scope: None,
            restore_focus: None,
            hovered: None,
            pressed: None,
        }
    }

    /// Return the committed scene.
    #[must_use]
    pub const fn scene(&self) -> &HitMap {
        &self.scene
    }

    /// Return active focus.
    #[must_use]
    pub fn focused(&self) -> Option<&FocusId> {
        self.focus.active()
    }

    /// Return active hover target.
    #[must_use]
    pub const fn hovered(&self) -> Option<&HitId> {
        self.hovered.as_ref()
    }

    /// Replace routing metadata after a successful frame commit.
    pub fn commit_scene(&mut self, scene: HitMap, scope: Option<FocusScopeId>) {
        let previous_focus = self.focus.active().cloned();
        let scope_changed = self.focus_scope != scope;
        if scope_changed && scope.is_some() {
            self.restore_focus.clone_from(&previous_focus);
        }
        let preferred_focus = if scope_changed && scope.is_none() {
            self.restore_focus.take().or(previous_focus)
        } else {
            previous_focus
        };
        self.scene = scene;
        self.focus_scope = scope;
        self.focus = FocusTrap::from_hits(
            &self.scene,
            self.focus_scope.as_ref(),
            preferred_focus.as_ref(),
        );
        if self
            .hovered
            .as_ref()
            .is_some_and(|id| !contains_enabled(&self.scene, id))
        {
            self.hovered = None;
        }
        if self
            .pressed
            .as_ref()
            .is_some_and(|id| !contains_enabled(&self.scene, id))
        {
            self.pressed = None;
        }
    }

    /// Set active focus to a committed target.
    pub fn set_focused(&mut self, id: &FocusId) -> bool {
        self.focus.set_active(id)
    }

    /// Route one event using generic webpage-like traversal semantics.
    pub fn route(&mut self, event: Event) -> InteractionRoute {
        if let Event::Key(stroke) = &event
            && is_tab_traversal(*stroke)
        {
            let focus_changed = match self.focus.handle_key(*stroke) {
                FocusKeyOutcome::Moved(id) => Some(id),
                FocusKeyOutcome::Ignored => None,
            };
            return InteractionRoute {
                event,
                target: focus_changed.clone(),
                hover_left: None,
                hover_entered: None,
                traversal_consumed: focus_changed.is_some(),
                focus_changed,
            };
        }

        match event {
            Event::Mouse(mouse) => {
                let hit = self
                    .scene
                    .hit_mouse_in_scope(mouse, self.focus_scope.as_ref())
                    .map(|hit| hit.id().clone());
                let mut hover_left = None;
                let mut hover_entered = None;
                let mut focus_changed = None;
                if matches!(mouse.kind, MouseEventKind::Move | MouseEventKind::Drag(_))
                    && self.hovered != hit
                {
                    hover_left.clone_from(&self.hovered);
                    hover_entered.clone_from(&hit);
                    self.hovered.clone_from(&hit);
                }
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.pressed.clone_from(&hit);
                        if let Some(id) = hit.as_ref()
                            && self.focus.set_active(id)
                            && self.focus.active() == Some(id)
                        {
                            focus_changed = Some(id.clone());
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => self.pressed = None,
                    MouseEventKind::Down(_)
                    | MouseEventKind::Up(_)
                    | MouseEventKind::Drag(_)
                    | MouseEventKind::Move
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => {}
                }
                InteractionRoute {
                    event: Event::Mouse(mouse),
                    target: hit,
                    hover_left,
                    hover_entered,
                    focus_changed,
                    traversal_consumed: false,
                }
            }
            Event::Key(stroke) => InteractionRoute {
                event: Event::Key(stroke),
                target: self.focus.active().cloned(),
                hover_left: None,
                hover_entered: None,
                focus_changed: None,
                traversal_consumed: false,
            },
            Event::Resize(size) => untargeted(Event::Resize(size)),
            Event::Paste(text) => InteractionRoute {
                event: Event::Paste(text),
                target: self.focus.active().cloned(),
                hover_left: None,
                hover_entered: None,
                focus_changed: None,
                traversal_consumed: false,
            },
            Event::Focus(focus) => untargeted(Event::Focus(focus)),
            Event::Tick => untargeted(Event::Tick),
            Event::User(value) => untargeted(Event::User(value)),
        }
    }
}

fn is_tab_traversal(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Tab && !stroke.modifiers.ctrl && !stroke.modifiers.alt
}

fn contains_enabled(scene: &HitMap, id: &HitId) -> bool {
    scene
        .regions()
        .iter()
        .any(|region| region.enabled && &region.id == id)
}

const fn untargeted(event: Event) -> InteractionRoute {
    InteractionRoute {
        event,
        target: None,
        hover_left: None,
        hover_entered: None,
        focus_changed: None,
        traversal_consumed: false,
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    use super::InteractionRouter;
    use crate::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use crate::geometry::{Point, Rect};
    use crate::hit::{HitId, HitMap, HitRegion};

    fn scene() -> HitMap {
        HitMap::new()
            .with_region(
                HitRegion::new("first", Rect::new(2, 3, 5, 1))
                    .hoverable(true)
                    .focusable(true),
            )
            .with_region(
                HitRegion::new("second", Rect::new(10, 3, 5, 1))
                    .hoverable(true)
                    .focusable(true),
            )
    }

    #[test]
    fn tab_and_backtab_follow_committed_render_order() {
        let mut router = InteractionRouter::new();
        router.commit_scene(scene(), None);

        let forward = router.route(Event::Key(KeyStroke::simple(KeyCode::Tab)));
        let backward = router.route(Event::Key(KeyStroke::with_modifiers(
            KeyCode::Tab,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        )));

        assert_eq!(forward.focus_changed, Some(HitId::new("second")));
        assert!(forward.traversal_consumed);
        assert_eq!(backward.focus_changed, Some(HitId::new("first")));
    }

    #[test]
    fn one_move_reports_both_hover_leave_and_enter() {
        let mut router = InteractionRouter::new();
        router.commit_scene(scene(), None);
        router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(3, 3),
        )));

        let route = router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(11, 3),
        )));

        assert_eq!(route.hover_left, Some(HitId::new("first")));
        assert_eq!(route.hover_entered, Some(HitId::new("second")));
        assert!(route.needs_redraw());
    }

    #[test]
    fn click_transfers_keyboard_focus_to_exact_hit_target() {
        let mut router = InteractionRouter::new();
        router.commit_scene(scene(), None);

        let route = router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(11, 3),
        )));

        assert_eq!(route.target, Some(HitId::new("second")));
        assert_eq!(route.focus_changed, Some(HitId::new("second")));
        assert_eq!(router.focused(), Some(&HitId::new("second")));
    }

    #[test]
    fn modal_scope_blocks_background_pointer_and_restores_focus() {
        let background = HitRegion::new("background", Rect::new(0, 0, 20, 5))
            .hoverable(true)
            .focusable(true);
        let modal = HitRegion::new("modal.close", Rect::new(8, 2, 5, 1))
            .hoverable(true)
            .focusable(true)
            .focus_scope("modal");
        let mut router = InteractionRouter::new();
        router.commit_scene(HitMap::new().with_region(background.clone()), None);
        assert_eq!(router.focused(), Some(&HitId::new("background")));

        router.commit_scene(
            HitMap::new()
                .with_region(background.clone())
                .with_region(modal),
            Some(HitId::new("modal")),
        );
        assert_eq!(router.focused(), Some(&HitId::new("modal.close")));
        let blocked = router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(1, 1),
        )));
        assert_eq!(blocked.target, None);

        router.commit_scene(HitMap::new().with_region(background), None);
        assert_eq!(router.focused(), Some(&HitId::new("background")));
    }

    #[test]
    fn committed_scene_reconciles_removed_hover_and_focus() {
        let mut router = InteractionRouter::new();
        router.commit_scene(scene(), None);
        router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(11, 3),
        )));
        router.route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(11, 3),
        )));

        router.commit_scene(
            HitMap::new().with_region(
                HitRegion::new("first", Rect::new(0, 0, 1, 1))
                    .hoverable(true)
                    .focusable(true),
            ),
            None,
        );

        assert_eq!(router.hovered(), None);
        assert_eq!(router.focused(), Some(&HitId::new("first")));
    }
}
