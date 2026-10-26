//! Pointer presentation uses only the last successfully displayed compositor.

use bmux_attach_pipeline::pointer_shape::{
    PointerShapeInput, ResolvedPointerShape, resolve_pointer_shape,
};
use bmux_terminal_protocol::pointer_shape::PointerShape;

use super::state::AttachViewState;

pub(super) fn resolve(view: &AttachViewState) -> Option<ResolvedPointerShape> {
    if !view.mouse.config.pointer_shapes {
        return None;
    }
    let gesture = view.mouse.resize_drag.map_or_else(
        || {
            if view.mouse.floating_drag.is_some() {
                Some(PointerShape::Grabbing)
            } else if view.mouse.selection_drag.is_some() {
                Some(PointerShape::Text)
            } else {
                None
            }
        },
        |resize| {
            Some(
                match (resize.horizontal.is_some(), resize.vertical.is_some()) {
                    (true, true) => PointerShape::NwseResize,
                    (true, false) => PointerShape::EwResize,
                    (false, true) => PointerShape::NsResize,
                    (false, false) => PointerShape::Default,
                },
            )
        },
    );
    let hover = view.mouse.last_position.and_then(|(x, y)| {
        // Never override capture or eligible retained chrome.
        if view.plugin_pointer_router.captured().is_some()
            || view.mouse.input_capture.is_some()
            || view.retained_compositor.hit_test(x, y).is_some_and(|hit| {
                view.retained_compositor
                    .surfaces()
                    .get(&hit.surface_id)
                    .is_some_and(|surface| surface.owner.is_some())
            })
        {
            return None;
        }
        super::runtime::committed_pointer_border_intent(view, x, y)
    });
    Some(resolve_pointer_shape(
        PointerShapeInput {
            compositor: &view.retained_compositor,
            router: &view.plugin_pointer_router,
            position: view.mouse.last_position,
            gesture: gesture.or(hover),
            fallback: PointerShape::Default,
        },
        |content| {
            // A historical scrollback projection is not the live terminal's
            // current application surface.
            if view.scrollback_active_for(content) {
                return None;
            }
            let tracker = &view.pane_buffers.get(&content)?.protocol_tracker;
            tracker
                .protocol_state()
                .pointer_shapes
                .current(tracker.alternate_screen())
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_pipeline::render::{DamageCoalescingPolicy, DamageRect};
    use bmux_attach_pipeline::{PaneRenderBuffer, RetainedSurface};
    use bmux_client::AttachOpenInfo;
    use uuid::Uuid;

    fn view() -> (AttachViewState, Uuid) {
        let content = Uuid::from_u128(22);
        let mut view = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::from_u128(1),
            can_write: true,
        });
        view.mouse.config.pointer_shapes = true;
        view.mouse.last_position = Some((2, 2));
        view.retained_compositor.replace_surfaces(
            vec![
                RetainedSurface::builder(Uuid::from_u128(2), DamageRect::new(0, 0, 10, 5))
                    .opaque()
                    .content(content)
                    .build(),
            ],
            DamageRect::new(0, 0, 10, 5),
            DamageCoalescingPolicy::default(),
        );
        view.pane_buffers
            .insert(content, PaneRenderBuffer::default());
        (view, content)
    }

    #[test]
    fn stationary_content_updates_and_replacement_do_not_require_mouse_reporting() {
        let (mut view, content) = view();
        view.mouse.config.enabled = false;
        let buffer = view.pane_buffers.get_mut(&content).unwrap();
        super::super::render::append_pane_output(buffer, b"\x1b]22;pointer\x1b\\");
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Pointer))
        );
        super::super::render::append_pane_output(
            view.pane_buffers.get_mut(&content).unwrap(),
            b"\x1b]22;wait\x1b\\",
        );
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Wait))
        );
        view.pane_buffers
            .insert(content, PaneRenderBuffer::default());
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Text))
        );
        view.retained_compositor.replace_surfaces(
            Vec::new(),
            DamageRect::new(0, 0, 10, 5),
            DamageCoalescingPolicy::default(),
        );
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Default))
        );
    }

    #[test]
    fn alternate_screen_and_configuration_remain_independent() {
        let (mut view, content) = view();
        super::super::render::append_pane_output(
            view.pane_buffers.get_mut(&content).unwrap(),
            b"\x1b]22;pointer\x1b\\\x1b[?1049h\x1b]22;crosshair\x1b\\",
        );
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Crosshair))
        );
        super::super::render::append_pane_output(
            view.pane_buffers.get_mut(&content).unwrap(),
            b"\x1b[?1049l",
        );
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::Pointer))
        );
        view.mouse.config.pointer_shapes = false;
        assert_eq!(resolve(&view), None);
    }

    #[test]
    fn resize_gesture_wins_over_content_without_a_pointer_position() {
        let (mut view, _) = view();
        view.mouse.last_position = None;
        let axis = super::super::state::AttachMouseResizeAxisDrag {
            positive_target_pane_id: Uuid::from_u128(1),
            positive_direction: bmux_tabs_plugin_api::tabs_commands::PaneResizeDirection::Right,
            negative_target_pane_id: Uuid::from_u128(2),
            negative_direction: bmux_tabs_plugin_api::tabs_commands::PaneResizeDirection::Left,
        };
        view.mouse.resize_drag = Some(super::super::state::AttachMouseResizeDrag {
            horizontal: Some(axis),
            vertical: None,
            last_column: 0,
            last_row: 0,
            latest_column: 0,
            latest_row: 0,
            last_applied_at: std::time::Instant::now(),
        });
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::EwResize))
        );
        view.mouse.resize_drag.as_mut().unwrap().vertical = Some(axis);
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::NwseResize))
        );
        view.mouse.resize_drag.as_mut().unwrap().horizontal = None;
        assert_eq!(
            resolve(&view),
            Some(ResolvedPointerShape::Shape(PointerShape::NsResize))
        );
    }
}
