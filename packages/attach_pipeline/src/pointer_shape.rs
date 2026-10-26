//! Pure pointer presentation resolution over a caller's committed retained scene.
//!
//! No output, capability detection, resource lookup, or capture mutation occurs
//! here. Consumers must recompute after scene/state changes, not just movement.

use bmux_plugin::surface::PluginSurfaceCursor;
use bmux_terminal_protocol::pointer_shape::PointerShape;
use uuid::Uuid;

use crate::compositor::{RetainedCompositor, RetainedPointerRouter, RetainedPointerTarget};

/// A hidden pointer is an intent, not an OSC 22 shape. Consumers must choose a
/// truthful supported fallback when their output transport cannot express it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedPointerShape {
    Shape(PointerShape),
    Hidden,
}

/// Attachment-local inputs. The compositor must describe committed output;
/// `gesture` comes from the caller's existing normalized gesture owner.
#[derive(Clone, Copy, Debug)]
pub struct PointerShapeInput<'a> {
    pub compositor: &'a RetainedCompositor,
    pub router: &'a RetainedPointerRouter,
    pub position: Option<(u16, u16)>,
    pub gesture: Option<PointerShape>,
    pub fallback: PointerShape,
}

/// Resolve gesture, valid capture, eligible chrome, visible content, then fallback.
///
/// The callback receives an opaque retained content identity, never an execution
/// or domain contract. Missing content intent means text. Explicit chrome
/// default is authoritative and does not inherit an underlying content shape.
#[must_use]
pub fn resolve_pointer_shape(
    input: PointerShapeInput<'_>,
    mut content_shape: impl FnMut(Uuid) -> Option<PointerShape>,
) -> ResolvedPointerShape {
    if let Some(shape) = input.gesture {
        return ResolvedPointerShape::Shape(shape);
    }
    if let Some(cursor) = input.router.captured_cursor(input.compositor) {
        return resolve_cursor(cursor, true);
    }
    let target = input
        .position
        .and_then(|(x, y)| input.compositor.pointer_target(x, y));
    match target {
        Some(RetainedPointerTarget::Cursor(cursor)) => resolve_cursor(cursor, false),
        Some(RetainedPointerTarget::Content(id)) => {
            ResolvedPointerShape::Shape(content_shape(id).unwrap_or(PointerShape::Text))
        }
        Some(RetainedPointerTarget::Blocked) | None => ResolvedPointerShape::Shape(input.fallback),
    }
}

const fn resolve_cursor(cursor: PluginSurfaceCursor, captured: bool) -> ResolvedPointerShape {
    let shape = match cursor {
        PluginSurfaceCursor::Default => PointerShape::Default,
        PluginSurfaceCursor::Pointer => PointerShape::Pointer,
        PluginSurfaceCursor::Text => PointerShape::Text,
        PluginSurfaceCursor::Crosshair => PointerShape::Crosshair,
        PluginSurfaceCursor::ResizeHorizontal => PointerShape::EwResize,
        PluginSurfaceCursor::ResizeVertical => PointerShape::NsResize,
        PluginSurfaceCursor::Hidden => return ResolvedPointerShape::Hidden,
        PluginSurfaceCursor::Tui { intent, role } => {
            return match intent.resolve(role, captured) {
                bmux_tui::pointer::PointerIntent::Shape(shape) => {
                    ResolvedPointerShape::Shape(shape)
                }
                bmux_tui::pointer::PointerIntent::Hidden => ResolvedPointerShape::Hidden,
                // resolve() always lowers Auto to a concrete shape.
                bmux_tui::pointer::PointerIntent::Auto => {
                    ResolvedPointerShape::Shape(PointerShape::Default)
                }
            };
        }
    };
    ResolvedPointerShape::Shape(shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::{
        RetainedInteractiveRegion, RetainedOpacity, RetainedSurface, RetainedSurfacePayload,
    };
    use crate::render::{DamageCoalescingPolicy, DamageRect};
    use bmux_plugin::surface::{PluginSurfaceId, PluginSurfaceRegionId};

    const CONTENT: Uuid = Uuid::from_u128(10);

    fn content() -> RetainedSurface {
        RetainedSurface::builder(Uuid::from_u128(1), DamageRect::new(0, 0, 20, 10))
            .opaque()
            .content(CONTENT)
            .build()
    }

    fn chrome(cursor: PluginSurfaceCursor) -> RetainedSurface {
        let id = Uuid::from_u128(2);
        let owner = PluginSurfaceId::new("test", "chrome", id);
        let mut surface = RetainedSurface::builder(id, DamageRect::new(2, 2, 6, 3))
            .z(1)
            .transparent()
            .render_ops(Vec::new())
            .revision(1)
            .build();
        surface.interactive_regions.push(RetainedInteractiveRegion {
            id: Some(PluginSurfaceRegionId::new(&owner, "action")),
            rect: surface.rect,
            focusable: false,
            cursor,
            endpoint: None,
        });
        surface
    }

    fn scene(surfaces: Vec<RetainedSurface>) -> RetainedCompositor {
        let mut compositor = RetainedCompositor::new();
        replace(&mut compositor, surfaces);
        compositor
    }

    fn replace(compositor: &mut RetainedCompositor, surfaces: Vec<RetainedSurface>) {
        compositor.replace_surfaces(
            surfaces,
            DamageRect::new(0, 0, 20, 10),
            DamageCoalescingPolicy::default(),
        );
    }

    fn input<'a>(
        compositor: &'a RetainedCompositor,
        router: &'a RetainedPointerRouter,
        position: Option<(u16, u16)>,
    ) -> PointerShapeInput<'a> {
        PointerShapeInput {
            compositor,
            router,
            position,
            gesture: None,
            fallback: PointerShape::Default,
        }
    }

    #[test]
    fn content_is_opaque_identity_scoped_and_missing_intent_is_text() {
        let compositor = scene(vec![content()]);
        let router = RetainedPointerRouter::default();
        let at = input(&compositor, &router, Some((0, 0)));
        assert_eq!(
            resolve_pointer_shape(at, |id| {
                assert_eq!(id, CONTENT);
                Some(PointerShape::Crosshair)
            }),
            ResolvedPointerShape::Shape(PointerShape::Crosshair)
        );
        assert_eq!(
            resolve_pointer_shape(at, |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
        for position in [None, Some((20, 0)), Some((0, 10))] {
            assert_eq!(
                resolve_pointer_shape(input(&compositor, &router, position), |_| {
                    panic!("offscreen content queried")
                }),
                ResolvedPointerShape::Shape(PointerShape::Default)
            );
        }
    }

    #[test]
    fn explicit_chrome_intents_never_fall_through_to_content() {
        for (cursor, expected) in [
            (
                PluginSurfaceCursor::Default,
                ResolvedPointerShape::Shape(PointerShape::Default),
            ),
            (
                PluginSurfaceCursor::Pointer,
                ResolvedPointerShape::Shape(PointerShape::Pointer),
            ),
            (
                PluginSurfaceCursor::Text,
                ResolvedPointerShape::Shape(PointerShape::Text),
            ),
            (
                PluginSurfaceCursor::Crosshair,
                ResolvedPointerShape::Shape(PointerShape::Crosshair),
            ),
            (
                PluginSurfaceCursor::ResizeHorizontal,
                ResolvedPointerShape::Shape(PointerShape::EwResize),
            ),
            (
                PluginSurfaceCursor::ResizeVertical,
                ResolvedPointerShape::Shape(PointerShape::NsResize),
            ),
            (PluginSurfaceCursor::Hidden, ResolvedPointerShape::Hidden),
        ] {
            let compositor = scene(vec![content(), chrome(cursor)]);
            assert_eq!(
                resolve_pointer_shape(
                    input(&compositor, &RetainedPointerRouter::default(), Some((3, 3))),
                    |_| panic!("explicit chrome fell through"),
                ),
                expected
            );
        }
    }

    #[test]
    fn tui_shapes_and_hidden_survive_hover_and_capture() {
        use bmux_tui::hit::HitRole;
        use bmux_tui::pointer::PointerIntent;

        for intent in PointerShape::ALL
            .into_iter()
            .map(PointerIntent::Shape)
            .chain([PointerIntent::Hidden])
        {
            let compositor = scene(vec![
                content(),
                chrome(PluginSurfaceCursor::Tui {
                    intent,
                    role: HitRole::DragHandle,
                }),
            ]);
            let mut router = RetainedPointerRouter::default();
            let expected = match intent {
                PointerIntent::Shape(shape) => ResolvedPointerShape::Shape(shape),
                PointerIntent::Hidden => ResolvedPointerShape::Hidden,
                PointerIntent::Auto => unreachable!(),
            };
            assert_eq!(
                resolve_pointer_shape(input(&compositor, &router, Some((3, 3))), |_| panic!(
                    "chrome fell through"
                )),
                expected
            );
            router.capture(compositor.hit_test(3, 3).unwrap());
            assert_eq!(
                resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| panic!(
                    "capture fell through"
                )),
                expected
            );
        }
    }

    #[test]
    fn plugin_lowering_preserves_tui_metadata_and_disabled_visual_occlusion() {
        use bmux_plugin::ExtensionRect;
        use bmux_plugin::layout::PluginLayoutId;
        use bmux_plugin::surface::{PluginSurface, PluginSurfaceRegion, PluginSurfaceTarget};
        use bmux_tui::geometry::Rect;
        use bmux_tui::hit::{HitRegion, HitRole};
        use bmux_tui::pointer::PointerIntent;
        use std::collections::BTreeMap;

        let mut hit = HitRegion::new("control", Rect::new(0, 0, 4, 1))
            .role(HitRole::DragHandle)
            .pointer_intent(PointerIntent::Shape(PointerShape::Grab));
        for enabled in [true, false] {
            hit.enabled = enabled;
            let mut plugin_surface = PluginSurface::layout(
                PluginSurfaceId::new("test", "control", Uuid::from_u128(8)),
                1,
                PluginLayoutId::new("test", "allocation"),
                Vec::new(),
            )
            .interactive_region(PluginSurfaceRegion::from_tui(&hit))
            .order(0, 1);
            plugin_surface.target = PluginSurfaceTarget::Explicit(ExtensionRect::new(2, 2, 4, 1));
            let lowered = crate::compositor::retained_surfaces_from_plugin_surfaces(
                &[plugin_surface],
                &BTreeMap::new(),
                DamageRect::new(0, 0, 20, 10),
            );
            let mut surfaces = vec![content()];
            surfaces.extend(lowered);
            let compositor = scene(surfaces);
            assert_eq!(
                resolve_pointer_shape(
                    input(&compositor, &RetainedPointerRouter::default(), Some((3, 2))),
                    |_| panic!("control borrowed underlying content intent"),
                ),
                ResolvedPointerShape::Shape(if enabled {
                    PointerShape::Grab
                } else {
                    PointerShape::Default
                })
            );
        }
    }

    #[test]
    fn tui_auto_role_defaults_match_legacy_hover_meaning() {
        use bmux_tui::hit::HitRole;
        use bmux_tui::pointer::PointerIntent;

        for (role, expected) in [
            (HitRole::Action, PointerShape::Pointer),
            (HitRole::ListItem, PointerShape::Pointer),
            (HitRole::DragHandle, PointerShape::Pointer),
            (HitRole::TextInput, PointerShape::Text),
            (HitRole::ResizeHandle, PointerShape::EwResize),
            (HitRole::Scroll, PointerShape::Default),
            (HitRole::Background, PointerShape::Default),
            (HitRole::Custom(9), PointerShape::Default),
        ] {
            assert_eq!(
                resolve_cursor(
                    PluginSurfaceCursor::Tui {
                        intent: PointerIntent::Auto,
                        role
                    },
                    false
                ),
                ResolvedPointerShape::Shape(expected)
            );
        }
    }

    #[test]
    fn tui_auto_resolves_role_after_capture_validation() {
        use bmux_tui::hit::HitRole;
        use bmux_tui::pointer::PointerIntent;

        let mut compositor = scene(vec![
            content(),
            chrome(PluginSurfaceCursor::Tui {
                intent: PointerIntent::Auto,
                role: HitRole::DragHandle,
            }),
        ]);
        let mut router = RetainedPointerRouter::default();
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((3, 3))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Pointer)
        );
        router.capture(compositor.hit_test(3, 3).unwrap());
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Grabbing)
        );
        replace(&mut compositor, vec![content()]);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
    }

    #[test]
    fn opaque_unknown_and_modal_padding_block_underlying_intent() {
        for opacity in [
            RetainedOpacity::Opaque,
            RetainedOpacity::Unknown,
            RetainedOpacity::Transparent,
        ] {
            for modal in [false, true] {
                let mut overlay = chrome(PluginSurfaceCursor::Pointer);
                overlay.interactive_regions.clear();
                overlay.opacity = opacity;
                overlay.opaque = opacity.is_opaque();
                overlay.modal = modal;
                let compositor = scene(vec![content(), overlay]);
                let expected = if modal || opacity != RetainedOpacity::Transparent {
                    PointerShape::Default
                } else {
                    PointerShape::Text
                };
                assert_eq!(
                    resolve_pointer_shape(
                        input(&compositor, &RetainedPointerRouter::default(), Some((3, 3))),
                        |_| None,
                    ),
                    ResolvedPointerShape::Shape(expected)
                );
            }
        }
    }

    #[test]
    fn clipping_boundaries_and_stationary_replacement_use_current_scene() {
        let mut overlay = chrome(PluginSurfaceCursor::Pointer);
        overlay.clip_rect = Some(DamageRect::new(3, 3, 1, 1));
        let mut compositor = scene(vec![content(), overlay]);
        let router = RetainedPointerRouter::default();
        for (position, expected) in [
            ((3, 3), PointerShape::Pointer),
            ((2, 3), PointerShape::Text),
            ((4, 3), PointerShape::Text),
            ((3, 4), PointerShape::Text),
        ] {
            assert_eq!(
                resolve_pointer_shape(input(&compositor, &router, Some(position)), |_| None),
                ResolvedPointerShape::Shape(expected)
            );
        }
        replace(&mut compositor, vec![content()]);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((3, 3))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
    }

    #[test]
    fn gesture_precedes_capture_and_capture_survives_leaving_region() {
        let compositor = scene(vec![content(), chrome(PluginSurfaceCursor::Pointer)]);
        let mut router = RetainedPointerRouter::default();
        router.capture(compositor.hit_test(3, 3).unwrap());
        let mut at = input(&compositor, &router, Some((15, 8)));
        assert_eq!(
            resolve_pointer_shape(at, |_| panic!("capture fell through")),
            ResolvedPointerShape::Shape(PointerShape::Pointer)
        );
        at.gesture = Some(PointerShape::Grabbing);
        assert_eq!(
            resolve_pointer_shape(at, |_| panic!("gesture fell through")),
            ResolvedPointerShape::Shape(PointerShape::Grabbing)
        );
        router.release_capture();
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
    }

    #[test]
    fn capture_retargets_only_when_explicitly_preserved() {
        let mut compositor = scene(vec![content(), chrome(PluginSurfaceCursor::Pointer)]);
        let mut router = RetainedPointerRouter::default();
        router.capture(compositor.hit_test(3, 3).unwrap());
        let mut next = chrome(PluginSurfaceCursor::Crosshair);
        next.revision = Some(2);
        replace(&mut compositor, vec![content(), next.clone()]);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
        router.preserve_capture_across_revisions(true);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Crosshair)
        );
        next.interactive_regions[0]
            .id
            .as_mut()
            .unwrap()
            .region_local_id = "replacement".to_owned();
        replace(&mut compositor, vec![content(), next]);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Text)
        );
    }

    #[test]
    fn removed_clipped_or_occluded_capture_cannot_override_scene() {
        let mut compositor = scene(vec![content(), chrome(PluginSurfaceCursor::Pointer)]);
        let mut router = RetainedPointerRouter::default();
        router.capture(compositor.hit_test(3, 3).unwrap());
        for mode in 0..5 {
            let mut captured = chrome(PluginSurfaceCursor::Pointer);
            let mut surfaces = vec![content()];
            if mode == 1 {
                captured.clip_rect = Some(DamageRect::new(0, 0, 1, 1));
            }
            if mode != 0 {
                surfaces.push(captured);
            }
            if mode >= 2 {
                let mut overlay = RetainedSurface::with_payload(
                    Uuid::from_u128(3),
                    DamageRect::new(2, 2, 6, 3),
                    0,
                    2,
                    RetainedOpacity::Opaque,
                    RetainedSurfacePayload::RenderOps(Vec::new()),
                );
                if mode == 3 {
                    overlay.opacity = RetainedOpacity::Transparent;
                    overlay.opaque = false;
                    overlay.modal = true;
                    overlay.rect = DamageRect::new(10, 0, 1, 1);
                } else if mode == 4 {
                    overlay.rect.w = 3;
                    let mut other = overlay.clone();
                    other.id = Uuid::from_u128(4);
                    other.rect.x = 5;
                    surfaces.push(other);
                }
                surfaces.push(overlay);
            }
            replace(&mut compositor, surfaces);
            assert_ne!(
                resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
                ResolvedPointerShape::Shape(PointerShape::Pointer)
            );
        }
    }

    #[test]
    fn visible_top_content_wins_and_cached_hover_is_not_authority() {
        let mut compositor = scene(vec![content(), chrome(PluginSurfaceCursor::Pointer)]);
        let mut router = RetainedPointerRouter::default();
        let _ = router.route_move(&compositor, 3, 3);
        let top_id = Uuid::from_u128(11);
        let top = RetainedSurface::builder(Uuid::from_u128(3), DamageRect::new(0, 0, 10, 5))
            .z(2)
            .opaque()
            .content(top_id)
            .build();
        replace(&mut compositor, vec![content(), top]);
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((3, 3))), |id| {
                assert_eq!(id, top_id);
                Some(PointerShape::Wait)
            }),
            ResolvedPointerShape::Shape(PointerShape::Wait)
        );
    }

    #[test]
    fn partial_occlusion_preserves_capture_while_any_region_area_is_visible() {
        let mut compositor = scene(vec![content(), chrome(PluginSurfaceCursor::Pointer)]);
        let mut router = RetainedPointerRouter::default();
        router.capture(compositor.hit_test(3, 3).unwrap());
        let overlay = RetainedSurface::builder(Uuid::from_u128(3), DamageRect::new(2, 2, 3, 3))
            .z(2)
            .opaque()
            .render_ops(Vec::new())
            .build();
        replace(
            &mut compositor,
            vec![content(), chrome(PluginSurfaceCursor::Pointer), overlay],
        );
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((15, 8))), |_| None),
            ResolvedPointerShape::Shape(PointerShape::Pointer)
        );
    }

    #[test]
    fn decorated_scene_content_bounds_do_not_clip_paint_or_border_interaction() {
        use crate::compositor::retained_surfaces_from_attach_scene;
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface,
            AttachSurfaceKind, InteractiveRegion,
        };

        let outer = AttachRect {
            x: 2,
            y: 2,
            w: 8,
            h: 6,
        };
        let inner = AttachRect {
            x: 3,
            y: 3,
            w: 6,
            h: 4,
        };
        let mut scene_input = AttachScene {
            session_id: Uuid::from_u128(20),
            focus: AttachFocusTarget::None,
            surfaces: vec![AttachSurface {
                id: Uuid::from_u128(21),
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: outer,
                content_rect: inner,
                interactive_regions: vec![InteractiveRegion {
                    rect: AttachRect {
                        x: 2,
                        y: 2,
                        w: 8,
                        h: 1,
                    },
                    region_id: "title".to_owned(),
                    owning_plugin_id: "test".to_owned(),
                }],
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(CONTENT),
            }],
        };
        let lowered = retained_surfaces_from_attach_scene(&scene_input);
        assert_eq!(lowered[0].paint_rect(), Some(DamageRect::new(2, 2, 8, 6)));
        assert_eq!(lowered[0].content_rect, Some(DamageRect::new(3, 3, 6, 4)));
        assert_eq!(
            lowered[0].interactive_regions[0].rect,
            DamageRect::new(2, 2, 8, 1)
        );
        let compositor = scene(lowered);
        assert!(compositor.hit_test(3, 2).is_some());
        let router = RetainedPointerRouter::default();
        for position in [(3, 2), (2, 4), (9, 4), (3, 7)] {
            assert_eq!(
                resolve_pointer_shape(input(&compositor, &router, Some(position)), |_| {
                    panic!("border queried application pointer intent")
                }),
                ResolvedPointerShape::Shape(PointerShape::Default)
            );
        }
        assert_eq!(
            resolve_pointer_shape(input(&compositor, &router, Some((3, 3))), |_| {
                Some(PointerShape::Crosshair)
            }),
            ResolvedPointerShape::Shape(PointerShape::Crosshair)
        );
        scene_input.surfaces[0].content_rect.w = 0;
        let empty_content = scene(retained_surfaces_from_attach_scene(&scene_input));
        assert_eq!(
            resolve_pointer_shape(input(&empty_content, &router, Some((3, 3))), |_| {
                panic!("empty content queried application pointer intent")
            }),
            ResolvedPointerShape::Shape(PointerShape::Default)
        );
        scene_input.surfaces[0].content_rect = inner;
        scene_input.surfaces[0].accepts_input = false;
        let disabled = scene(retained_surfaces_from_attach_scene(&scene_input));
        assert_eq!(
            resolve_pointer_shape(input(&disabled, &router, Some((3, 3))), |_| {
                panic!("disabled surface queried application pointer intent")
            }),
            ResolvedPointerShape::Shape(PointerShape::Default)
        );
        scene_input.surfaces[0].visible = false;
        assert!(retained_surfaces_from_attach_scene(&scene_input).is_empty());
    }

    #[test]
    fn noninteractive_modal_occludes_without_publishing_chrome_intent() {
        let mut overlay = chrome(PluginSurfaceCursor::Pointer).with_accepts_input(false);
        overlay.modal = true;
        let compositor = scene(vec![content(), overlay]);
        assert_eq!(
            resolve_pointer_shape(
                input(&compositor, &RetainedPointerRouter::default(), Some((3, 3))),
                |_| panic!("noninteractive modal fell through"),
            ),
            ResolvedPointerShape::Shape(PointerShape::Default)
        );
    }

    #[test]
    fn anonymous_runtime_regions_do_not_override_content_shape() {
        let runtime = content().with_interactive_regions(vec![DamageRect::new(0, 0, 20, 10)]);
        let compositor = scene(vec![runtime]);
        assert_eq!(
            resolve_pointer_shape(
                input(&compositor, &RetainedPointerRouter::default(), Some((3, 3))),
                |_| Some(PointerShape::Crosshair),
            ),
            ResolvedPointerShape::Shape(PointerShape::Crosshair)
        );
    }
}
