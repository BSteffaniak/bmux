use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::prelude::Line;
use bmux_tui::style::Color;
use bmux_tui_components::common::{
    ComponentHitRegion, DragState, HitRegionId, ResizeBounds, hit_region_at,
};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::pane::{Pane, PaneBoundsPolicy, PaneMousePolicy, PanePolicy, PaneState};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 18;

pub fn render_layout_playground() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);

    let pane = Pane::new()
        .title("Bounded pane")
        .padding(Insets::all(1))
        .policy(PanePolicy {
            mouse: PaneMousePolicy::draggable(),
            bounds: PaneBoundsPolicy {
                parent: Some(Rect::new(0, 0, WIDTH, HEIGHT)),
                min_size: Size::new(8, 4),
                max_size: Some(Size::new(30, 10)),
            },
        });
    let pane_state = PaneState::new(Rect::new(2, 2, 30, 8));
    pane.render(&pane_state, &mut frame);
    frame.write_line(
        pane.inner_area(&pane_state),
        &Line::from("Drag and resize bounds"),
    );

    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(26, 7), Size::new(26, 7), Insets::all(0)),
        ModalTheme::dark(Color::Magenta),
    )
    .placement(ModalPlacement::UpperThird)
    .title("Upper third");
    modal.render(Rect::new(36, 1, 32, 14), &mut frame);
    modal.render_line(
        modal.content_area(Rect::new(36, 1, 32, 14)),
        &Line::from("Modal placement"),
        &mut frame,
    );

    buffer
}

pub fn demonstrate_drag_delta() -> (i32, i32) {
    DragState::new(Point::new(2, 3))
        .moved_to(Point::new(7, 1))
        .delta()
}

pub fn demonstrate_hit_region() -> Option<HitRegionId> {
    let regions = [
        ComponentHitRegion::new(HitRegionId(10), Rect::new(0, 0, 5, 5)),
        ComponentHitRegion::new(HitRegionId(20), Rect::new(10, 0, 5, 5)),
    ];
    hit_region_at(&regions, Point::new(11, 2))
}

pub fn demonstrate_resize_bounds() -> Size {
    ResizeBounds::new(Size::new(4, 4), Some(Size::new(12, 8))).clamp(Size::new(20, 2))
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use bmux_tui::geometry::Size;
    use bmux_tui_components::common::HitRegionId;

    use super::{
        demonstrate_drag_delta, demonstrate_hit_region, demonstrate_resize_bounds,
        render_layout_playground, rows,
    };

    #[test]
    fn layout_playground_renders_panes_and_modals() {
        let rendered = rows(&render_layout_playground()).join("\n");

        assert!(rendered.contains("Bounded pane"));
        assert!(rendered.contains("Modal placement"));
    }

    #[test]
    fn shared_geometry_helpers_are_demonstrated() {
        assert_eq!(demonstrate_drag_delta(), (5, -2));
        assert_eq!(demonstrate_hit_region(), Some(HitRegionId(20)));
        assert_eq!(demonstrate_resize_bounds(), Size::new(12, 4));
    }
}
