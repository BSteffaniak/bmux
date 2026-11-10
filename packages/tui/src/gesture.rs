//! Click-versus-drag recognition over committed interaction scenes.
//!
//! A left press followed by a release is a click when the pointer never
//! travelled beyond a [`ClickSlop`] from the press and both ends hit the same
//! interactive region. Travelling beyond the slop turns the gesture into a
//! drag. [`PointerGestureRecognizer::route_selection`] composes this with a
//! [`SelectionController`]: drag events reach the selection only once the
//! gesture is a drag, so a press on an interactive region inside selectable
//! content stays a click even though the selection armed on the press, and a
//! drag beyond the slop selects without activating the region.
//!
//! The recognizer reads only committed scenes ([`HitMap`],
//! [`SelectionScene`]); it never changes keyboard focus or hover state and
//! does not replace [`crate::interaction::InteractionRouter`].

use crate::event::{MouseButton, MouseEvent, MouseEventKind};
use crate::geometry::Point;
use crate::hit::{HitId, HitMap, HitRole};
use crate::selection::{SelectionController, SelectionOutcome, SelectionScene};

/// Pointer travel tolerated between press and release of a click.
///
/// Travel is measured per axis in cells from the press position: a gesture
/// stays a click while `|dx| <= columns` and `|dy| <= rows` for every drag
/// event and the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClickSlop {
    /// Columns of horizontal travel tolerated.
    pub columns: u16,
    /// Rows of vertical travel tolerated.
    pub rows: u16,
}

impl ClickSlop {
    /// No travel tolerated: any drag to another cell starts a drag.
    pub const NONE: Self = Self {
        columns: 0,
        rows: 0,
    };

    /// Create a slop of `columns` horizontal and `rows` vertical cells.
    #[must_use]
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }

    /// Whether `point` lies within this slop of `origin`.
    #[must_use]
    pub const fn contains(self, origin: Point, point: Point) -> bool {
        origin.x.abs_diff(point.x) <= self.columns && origin.y.abs_diff(point.y) <= self.rows
    }
}

impl Default for ClickSlop {
    /// One column of horizontal jitter and no vertical travel.
    fn default() -> Self {
        Self::new(1, 0)
    }
}

/// What one pointer event meant for the current gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerGesture {
    /// The event is not part of a left-button gesture.
    Ignored,
    /// A left press started a gesture; `region` is the interactive region
    /// under it, if any.
    Pressed {
        /// Interactive region under the press.
        region: Option<HitId>,
    },
    /// A drag event within the slop; the gesture can still be a click.
    Holding {
        /// Pointer position.
        position: Point,
    },
    /// The first drag event beyond the slop: the gesture became a drag.
    DragStarted {
        /// Press position.
        origin: Point,
        /// Pointer position.
        position: Point,
    },
    /// A later drag event of a drag gesture.
    Dragged {
        /// Pointer position.
        position: Point,
    },
    /// Release of a drag gesture.
    DragEnded {
        /// Release position.
        position: Point,
    },
    /// Release within the slop over the interactive region that was pressed.
    Click {
        /// Activated region.
        region: HitId,
        /// Release position.
        position: Point,
    },
    /// Release within the slop that does not activate a region (no
    /// interactive region was pressed, or the release left it).
    Tap {
        /// Release position.
        position: Point,
    },
    /// The gesture was abandoned by [`PointerGestureRecognizer::cancel`].
    Cancelled,
}

/// Result of [`PointerGestureRecognizer::route_selection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GestureRoute {
    /// Gesture meaning of the event.
    pub gesture: PointerGesture,
    /// Effect of the event on the selection controller.
    pub selection: SelectionOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Press {
    origin: Point,
    region: Option<HitId>,
    dragging: bool,
}

/// Caller-owned click-versus-drag state for the left pointer button.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerGestureRecognizer {
    slop: ClickSlop,
    press: Option<Press>,
}

impl PointerGestureRecognizer {
    /// Create an idle recognizer with `slop`.
    #[must_use]
    pub const fn new(slop: ClickSlop) -> Self {
        Self { slop, press: None }
    }

    /// Return the configured slop.
    #[must_use]
    pub const fn slop(&self) -> ClickSlop {
        self.slop
    }

    /// Whether a left press is in progress.
    #[must_use]
    pub const fn is_pressed(&self) -> bool {
        self.press.is_some()
    }

    /// Whether the gesture in progress travelled beyond the slop.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.press.as_ref().is_some_and(|press| press.dragging)
    }

    /// Abandon the gesture in progress.
    pub fn cancel(&mut self) -> PointerGesture {
        if self.press.take().is_some() {
            PointerGesture::Cancelled
        } else {
            PointerGesture::Ignored
        }
    }

    /// Classify one pointer event against the committed hit scene.
    ///
    /// `scope` is the active modal focus scope, as for
    /// [`HitMap::hit_mouse_in_scope`]. Interactive regions are enabled,
    /// visible, pointer-accepting regions whose role activates on click:
    /// [`HitRole::Action`], [`HitRole::ListItem`] and [`HitRole::Custom`].
    /// Text inputs, scroll areas, resize and drag handles, and backgrounds
    /// never produce [`PointerGesture::Click`].
    pub fn handle(
        &mut self,
        hits: &HitMap,
        scope: Option<&HitId>,
        mouse: MouseEvent,
    ) -> PointerGesture {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A press without a release abandons any earlier gesture.
                let region = interactive_region(hits, scope, mouse);
                self.press = Some(Press {
                    origin: mouse.position,
                    region: region.clone(),
                    dragging: false,
                });
                PointerGesture::Pressed { region }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(press) = self.press.as_mut() else {
                    return PointerGesture::Ignored;
                };
                if press.dragging {
                    PointerGesture::Dragged {
                        position: mouse.position,
                    }
                } else if self.slop.contains(press.origin, mouse.position) {
                    PointerGesture::Holding {
                        position: mouse.position,
                    }
                } else {
                    press.dragging = true;
                    PointerGesture::DragStarted {
                        origin: press.origin,
                        position: mouse.position,
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.press.take() else {
                    return PointerGesture::Ignored;
                };
                if press.dragging || !self.slop.contains(press.origin, mouse.position) {
                    return PointerGesture::DragEnded {
                        position: mouse.position,
                    };
                }
                match press.region {
                    Some(region)
                        if interactive_region(hits, scope, mouse).as_ref() == Some(&region) =>
                    {
                        PointerGesture::Click {
                            region,
                            position: mouse.position,
                        }
                    }
                    _ => PointerGesture::Tap {
                        position: mouse.position,
                    },
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => PointerGesture::Ignored,
        }
    }

    /// Classify one pointer event and route it to `selection` consistently.
    ///
    /// * A press arms the selection as usual.
    /// * Drag events within the slop are withheld from the selection, so it
    ///   stays armed and selects nothing.
    /// * The drag event that leaves the slop, and every later one, reach the
    ///   selection, which starts selecting from the press position.
    /// * A release that ends a drag completes the selection; a release that
    ///   is a click or tap releases the still-armed selection, which reports
    ///   [`SelectionOutcome::Click`] and keeps no selection.
    ///
    /// Events this recognizer ignores are passed to the selection unchanged.
    pub fn route_selection(
        &mut self,
        hits: &HitMap,
        scope: Option<&HitId>,
        selection: &mut SelectionController,
        scene: &SelectionScene,
        mouse: MouseEvent,
    ) -> GestureRoute {
        let gesture = self.handle(hits, scope, mouse);
        let selection = if matches!(gesture, PointerGesture::Holding { .. }) {
            SelectionOutcome::Ignored
        } else {
            // A click or tap releases a selection that never received a drag
            // event, which the controller reports as a click.
            selection.handle_mouse(scene, mouse)
        };
        GestureRoute { gesture, selection }
    }
}

/// The topmost pointer region under `mouse` when it activates on click.
fn interactive_region(hits: &HitMap, scope: Option<&HitId>, mouse: MouseEvent) -> Option<HitId> {
    let hit = hits.hit_mouse_in_scope(mouse, scope)?;
    matches!(
        hit.role(),
        HitRole::Action | HitRole::ListItem | HitRole::Custom(_)
    )
    .then(|| hit.id().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::hit::HitRegion;
    use crate::selection::{SelectionFragment, SelectionGesturePhase, SelectionScope};

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent::new(kind, Point::new(x, y))
    }

    fn down(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn drag(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Drag(MouseButton::Left), x, y)
    }

    fn up(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Up(MouseButton::Left), x, y)
    }

    /// A header region on row 0 over selectable text, and plain text on row 1.
    fn scenes() -> (HitMap, SelectionScene) {
        let hits = HitMap::new()
            .with_region(HitRegion::new("header", Rect::new(0, 0, 10, 1)))
            .with_region(
                HitRegion::new("background", Rect::new(0, 1, 10, 1)).role(HitRole::Background),
            );
        let mut scene = SelectionScene::default();
        scene.push_scope(SelectionScope::new("doc", Rect::new(0, 0, 10, 2)));
        for y in 0..2_u16 {
            for x in 0..10_u16 {
                let offset = usize::from(y) * 10 + usize::from(x);
                scene.push_fragment(SelectionFragment::new(
                    "doc",
                    "text",
                    Rect::new(x, y, 1, 1),
                    0,
                    offset..offset + 1,
                ));
            }
        }
        (hits, scene)
    }

    #[test]
    fn press_and_release_within_slop_clicks_the_pressed_region() {
        let (hits, _) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::default());
        assert_eq!(
            gestures.handle(&hits, None, down(3, 0)),
            PointerGesture::Pressed {
                region: Some(HitId::new("header"))
            }
        );
        assert_eq!(
            gestures.handle(&hits, None, drag(4, 0)),
            PointerGesture::Holding {
                position: Point::new(4, 0)
            }
        );
        assert_eq!(
            gestures.handle(&hits, None, up(4, 0)),
            PointerGesture::Click {
                region: HitId::new("header"),
                position: Point::new(4, 0)
            }
        );
        assert!(!gestures.is_pressed());
    }

    #[test]
    fn travel_beyond_slop_is_a_drag_even_when_it_returns() {
        let (hits, _) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::default());
        gestures.handle(&hits, None, down(3, 0));
        assert_eq!(
            gestures.handle(&hits, None, drag(6, 0)),
            PointerGesture::DragStarted {
                origin: Point::new(3, 0),
                position: Point::new(6, 0)
            }
        );
        assert!(gestures.is_dragging());
        assert_eq!(
            gestures.handle(&hits, None, drag(3, 0)),
            PointerGesture::Dragged {
                position: Point::new(3, 0)
            }
        );
        assert_eq!(
            gestures.handle(&hits, None, up(3, 0)),
            PointerGesture::DragEnded {
                position: Point::new(3, 0)
            }
        );
    }

    #[test]
    fn release_outside_slop_without_drag_events_is_a_drag() {
        let (hits, _) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::NONE);
        gestures.handle(&hits, None, down(3, 0));
        assert_eq!(
            gestures.handle(&hits, None, up(4, 0)),
            PointerGesture::DragEnded {
                position: Point::new(4, 0)
            }
        );
    }

    #[test]
    fn non_interactive_or_changed_targets_tap_instead_of_click() {
        let (hits, _) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::new(1, 1));
        assert_eq!(
            gestures.handle(&hits, None, down(3, 1)),
            PointerGesture::Pressed { region: None }
        );
        assert_eq!(
            gestures.handle(&hits, None, up(3, 1)),
            PointerGesture::Tap {
                position: Point::new(3, 1)
            }
        );
        // Released within slop but over another region: no activation.
        gestures.handle(&hits, None, down(3, 0));
        assert_eq!(
            gestures.handle(&hits, None, up(3, 1)),
            PointerGesture::Tap {
                position: Point::new(3, 1)
            }
        );
        // A modal scope hides the unscoped header.
        let scope = HitId::new("modal");
        assert_eq!(
            gestures.handle(&hits, Some(&scope), down(3, 0)),
            PointerGesture::Pressed { region: None }
        );
        assert_eq!(gestures.cancel(), PointerGesture::Cancelled);
        assert_eq!(gestures.cancel(), PointerGesture::Ignored);
        assert_eq!(
            gestures.handle(&hits, None, up(3, 0)),
            PointerGesture::Ignored
        );
    }

    #[test]
    fn click_through_an_armed_selection_activates_without_selecting() {
        let (hits, scene) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::default());
        let mut selection = SelectionController::new();
        let route = gestures.route_selection(&hits, None, &mut selection, &scene, down(3, 0));
        assert_eq!(route.selection, SelectionOutcome::Armed);
        // One cell of jitter would select one grapheme without the slop.
        let route = gestures.route_selection(&hits, None, &mut selection, &scene, drag(4, 0));
        assert_eq!(route.selection, SelectionOutcome::Ignored);
        assert_eq!(selection.phase(), SelectionGesturePhase::Armed);
        let route = gestures.route_selection(&hits, None, &mut selection, &scene, up(4, 0));
        assert_eq!(
            route.gesture,
            PointerGesture::Click {
                region: HitId::new("header"),
                position: Point::new(4, 0)
            }
        );
        assert_eq!(route.selection, SelectionOutcome::Click);
        assert!(selection.snapshot(&scene).is_none());
    }

    #[test]
    fn drag_beyond_slop_from_an_interactive_region_selects_without_click() {
        let (hits, scene) = scenes();
        let mut gestures = PointerGestureRecognizer::new(ClickSlop::default());
        let mut selection = SelectionController::new();
        gestures.route_selection(&hits, None, &mut selection, &scene, down(2, 0));
        let route = gestures.route_selection(&hits, None, &mut selection, &scene, drag(6, 1));
        assert!(matches!(route.gesture, PointerGesture::DragStarted { .. }));
        assert!(matches!(route.selection, SelectionOutcome::Changed { .. }));
        let route = gestures.route_selection(&hits, None, &mut selection, &scene, up(6, 1));
        assert_eq!(
            route.gesture,
            PointerGesture::DragEnded {
                position: Point::new(6, 1)
            }
        );
        assert_eq!(route.selection, SelectionOutcome::Completed);
        let snapshot = selection.snapshot(&scene).expect("completed selection");
        assert_eq!(snapshot.slices[0].source_range, 2..17);
    }
}
