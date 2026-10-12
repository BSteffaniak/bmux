//! Focused integration tests for the runtime's canonical mirror Component.
pub(crate) use bmux_presentation_runtime::MirrorComponent;

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presentation_runtime::mirror::{
        CanvasRect, MirrorFrame, WireCell, WireImage, WireImagePayload,
    };
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::{geometry::Rect, terminal::Terminal};
    #[test]
    fn fixed_mirror_contributes_native_image_scene() {
        let frame = MirrorFrame {
            surface_id: "demo".into(),
            slide_id: "s".into(),
            revision: 1,
            width: 2,
            height: 1,
            cells: vec![
                WireCell {
                    symbol: "x".into(),
                    width: 1,
                    continuation: false,
                    fg: None,
                    bg: None,
                    modifiers: 0
                };
                2
            ],
            images: vec![WireImage {
                key: "i".into(),
                destination: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                clip: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                payload: WireImagePayload::Pixels {
                    bytes: vec![255; 4],
                    width: 1,
                    height: 1,
                    rgba: true,
                },
            }],
        };
        let component = MirrorComponent::new(&frame, 16_384, 512 * 1024).unwrap();
        let layout = component.layout(Constraints::for_width(5), &mut LayoutCx::default());
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 5, 3));
        terminal.draw(|cx| component.paint(&layout, cx)).unwrap();
        assert_eq!(terminal.image_scene().placements().count(), 1);
        let placement = frame.placement(5, 3);
        assert_eq!(
            placement.input_point(placement.destination_x, placement.destination_y),
            Some((0, 0))
        );
    }
}
