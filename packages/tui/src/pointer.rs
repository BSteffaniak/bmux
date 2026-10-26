//! Pointer presentation derived from committed hit geometry, not an input router.
//!
//! Pointer shapes are separate from the text caret. `Hidden` is an intent for
//! capable hosts, not an OSC 22 shape name or permission to hide the caret.

pub use bmux_terminal_protocol::pointer_shape::PointerShape;

use crate::geometry::Point;
use crate::hit::{HitId, HitMap, HitRegion, HitRole};

/// Caller-owned pointer presentation for a hit region.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PointerIntent {
    /// Derive presentation from the region's role and capture state.
    #[default]
    Auto,
    /// Request an existing OSC 22/CSS shape.
    Shape(PointerShape),
    /// Request an invisible pointer, independently of text-caret visibility.
    Hidden,
}

impl PointerIntent {
    /// Resolve Auto using compatibility defaults shared by TUI consumers.
    /// Explicit Shape and Hidden overrides are preserved during capture.
    #[must_use]
    pub const fn resolve(self, role: HitRole, captured: bool) -> Self {
        match self {
            Self::Auto => Self::Shape(match role {
                HitRole::TextInput => PointerShape::Text,
                HitRole::ResizeHandle => PointerShape::EwResize,
                HitRole::DragHandle if captured => PointerShape::Grabbing,
                HitRole::Action | HitRole::ListItem | HitRole::DragHandle => PointerShape::Pointer,
                HitRole::Scroll | HitRole::Background | HitRole::Custom(_) => PointerShape::Default,
            }),
            intent => intent,
        }
    }
}

/// Resolve presentation using the existing committed scene and modal scope.
///
/// Returns Shape(Default) when the pointer position is unknown or has no valid
/// target; never returns Auto. The topmost visible region occludes lower
/// regions even when disabled, outside the active scope, or not hoverable.
/// Disabled/out-of-scope regions cannot supply overrides. Pointer presentation
/// does not require hover delivery or change mouse-event routing.
///
/// A valid capture retains its owner's intent outside its bounds. Callers pass
/// their existing capture identity, not a separately maintained pointer router.
/// Removed, disabled, invisible, empty, non-pointer, or out-of-scope capture
/// owners are ignored. Re-resolve after each successful scene commit even if
/// the pointer has not moved; never resolve against a speculative frame.
#[must_use]
pub fn resolve_pointer_intent(
    scene: &HitMap,
    position: Option<Point>,
    scope: Option<&HitId>,
    capture: Option<&HitId>,
) -> PointerIntent {
    let Some(position) = position else {
        return PointerIntent::Shape(PointerShape::Default);
    };
    if let Some(capture) = capture
        && let Some(region) = scene
            .regions()
            .iter()
            .enumerate()
            .filter(|(_, region)| {
                &region.id == capture
                    && region.visible
                    && !region.area.is_empty()
                    && region.enabled
                    && region.pointer_events
                    && region.focus_scope.as_ref() == scope
            })
            .max_by_key(|(index, region)| (region.layer, *index))
            .map(|(_, region)| region)
    {
        return region.pointer_intent.resolve(region.role, true);
    }
    scene.pointer_region(position).map_or(
        PointerIntent::Shape(PointerShape::Default),
        |region: &HitRegion| {
            if region.enabled && region.focus_scope.as_ref() == scope {
                region.pointer_intent.resolve(region.role, false)
            } else {
                PointerIntent::Shape(PointerShape::Default)
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{PointerIntent, PointerShape, resolve_pointer_intent};
    use crate::geometry::{Point, Rect};
    use crate::hit::{HitId, HitMap, HitRegion, HitRole};

    fn region(id: &str) -> HitRegion {
        HitRegion::new(id, Rect::new(0, 0, 4, 2))
    }

    fn resolve(scene: &HitMap) -> PointerIntent {
        resolve_pointer_intent(scene, Some(Point::new(1, 1)), None, None)
    }

    #[test]
    fn auto_preserves_compatibility_role_defaults() {
        let cases = [
            (HitRole::Action, PointerShape::Pointer),
            (HitRole::ListItem, PointerShape::Pointer),
            (HitRole::TextInput, PointerShape::Text),
            (HitRole::ResizeHandle, PointerShape::EwResize),
            (HitRole::DragHandle, PointerShape::Pointer),
            (HitRole::Scroll, PointerShape::Default),
            (HitRole::Background, PointerShape::Default),
            (HitRole::Custom(4), PointerShape::Default),
        ];
        for (role, shape) in cases {
            assert_eq!(
                PointerIntent::default().resolve(role, false),
                PointerIntent::Shape(shape)
            );
        }
    }

    #[test]
    fn all_protocol_shapes_and_hidden_override_roles_and_capture() {
        for intent in PointerShape::ALL
            .into_iter()
            .map(PointerIntent::Shape)
            .chain([PointerIntent::Hidden])
        {
            let scene = HitMap::new().with_region(region("target").pointer_intent(intent));
            assert_eq!(resolve(&scene), intent);
            assert_eq!(
                resolve_pointer_intent(
                    &scene,
                    Some(Point::new(50, 50)),
                    None,
                    Some(&HitId::new("target"))
                ),
                intent
            );
        }
    }

    #[test]
    fn disabled_and_non_hoverable_occluders_do_not_leak_lower_intent() {
        let scene = HitMap::new().with_region(region("text").role(HitRole::TextInput));
        let disabled = scene.clone().with_region(
            region("disabled")
                .enabled(false)
                .pointer_intent(PointerIntent::Hidden),
        );
        assert_eq!(
            resolve(&disabled),
            PointerIntent::Shape(PointerShape::Default)
        );
        let background = scene.with_region(
            region("cover")
                .role(HitRole::Background)
                .pointer_events(false),
        );
        assert_eq!(
            resolve(&background),
            PointerIntent::Shape(PointerShape::Default)
        );
    }

    #[test]
    fn geometry_obeys_layer_then_registration_and_ignores_invisible_regions() {
        let scene = HitMap::new()
            .with_region(region("high").layer(2).role(HitRole::TextInput))
            .with_region(region("low").layer(1))
            .with_region(region("hidden").layer(3).visible(false));
        assert_eq!(resolve(&scene), PointerIntent::Shape(PointerShape::Text));
        let scene = scene.with_region(
            region("later")
                .layer(2)
                .pointer_intent(PointerIntent::Hidden),
        );
        assert_eq!(resolve(&scene), PointerIntent::Hidden);
    }

    #[test]
    fn capture_uses_owner_and_invalid_capture_falls_back_to_geometry() {
        let id = HitId::new("drag");
        let scene = HitMap::new().with_region(region("drag").role(HitRole::DragHandle));
        let outside = Some(Point::new(20, 20));
        assert_eq!(
            resolve_pointer_intent(&scene, outside, None, Some(&id)),
            PointerIntent::Shape(PointerShape::Grabbing)
        );
        for invalid in [
            region("drag").enabled(false),
            region("drag").visible(false),
            region("drag").pointer_events(false),
            region("drag").focus_scope("modal"),
        ] {
            assert_eq!(
                resolve_pointer_intent(
                    &HitMap::new().with_region(invalid),
                    outside,
                    None,
                    Some(&id)
                ),
                PointerIntent::Shape(PointerShape::Default)
            );
        }
        assert_eq!(
            resolve_pointer_intent(&HitMap::new(), outside, None, Some(&id)),
            PointerIntent::Shape(PointerShape::Default)
        );
    }

    #[test]
    fn modal_scope_and_unknown_position_exclude_background_intent() {
        let scope = HitId::new("modal");
        let scene =
            HitMap::new().with_region(region("background").pointer_intent(PointerIntent::Hidden));
        assert_eq!(
            resolve_pointer_intent(&scene, Some(Point::new(1, 1)), Some(&scope), None),
            PointerIntent::Shape(PointerShape::Default)
        );
        let scene = scene.with_region(
            region("modal-target")
                .focus_scope(scope.clone())
                .role(HitRole::TextInput),
        );
        assert_eq!(
            resolve_pointer_intent(&scene, Some(Point::new(1, 1)), Some(&scope), None),
            PointerIntent::Shape(PointerShape::Text)
        );
        assert_eq!(
            resolve_pointer_intent(&scene, None, Some(&scope), None),
            PointerIntent::Shape(PointerShape::Default)
        );
    }
}
