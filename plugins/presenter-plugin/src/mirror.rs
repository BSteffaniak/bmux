//! Focused integration tests for the runtime's canonical mirror Component.
pub(crate) use bmux_presenter_runtime::MirrorComponent;

/// Private operator scene: cells and clipped targets use the embedded surface's
/// exact retained placement, never a separately measured rectangle.
pub(crate) struct OperatorMirror<'a> {
    pub mirror: MirrorComponent<'a>,
    pub scene: &'a bmux_presenter_runtime::mirror::MirrorPrivateScene,
}
impl bmux_tui::prelude::Component for OperatorMirror<'_> {
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.mirror.revision()
    }

    fn layout(
        &self,
        constraints: bmux_tui::prelude::Constraints,
        cx: &mut bmux_tui::prelude::LayoutCx,
    ) -> bmux_tui::prelude::LayoutNode {
        self.mirror.layout(constraints, cx)
    }
    fn paint(
        &self,
        layout: &bmux_tui::prelude::LayoutNode,
        cx: &mut bmux_tui::prelude::PaintCx<'_, '_>,
    ) {
        self.mirror.paint(layout, cx);
        let Some(placement) = self.mirror.placement(layout) else {
            return;
        };
        for target in &self.scene.interactions {
            let area = target.area;
            let left = area.x.max(placement.source_x);
            let top = area.y.max(placement.source_y);
            let right = area
                .x
                .saturating_add(area.width)
                .min(placement.source_x.saturating_add(placement.width));
            let bottom = area
                .y
                .saturating_add(area.height)
                .min(placement.source_y.saturating_add(placement.height));
            if left >= right || top >= bottom {
                continue;
            }
            cx.push_hit(
                bmux_tui::hit::HitRegion::new(
                    target.id.clone(),
                    bmux_tui::geometry::Rect::new(
                        placement.destination_x + left - placement.source_x,
                        placement.destination_y + top - placement.source_y,
                        right - left,
                        bottom - top,
                    ),
                )
                .role(bmux_tui::hit::HitRole::Action)
                .focusable(true)
                .enabled(target.enabled),
            );
        }
    }
}

impl bmux_tui::prelude::Component for &OperatorMirror<'_> {
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        (**self).revision()
    }

    fn layout(
        &self,
        constraints: bmux_tui::prelude::Constraints,
        cx: &mut bmux_tui::prelude::LayoutCx,
    ) -> bmux_tui::prelude::LayoutNode {
        (**self).layout(constraints, cx)
    }
    fn paint(
        &self,
        layout: &bmux_tui::prelude::LayoutNode,
        cx: &mut bmux_tui::prelude::PaintCx<'_, '_>,
    ) {
        (**self).paint(layout, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presenter_runtime::mirror::{
        CanvasRect, MirrorFrame, WireCell, WireImage, WireImagePayload,
    };
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::{geometry::Rect, terminal::Terminal};
    #[test]
    fn fixed_mirror_contributes_native_image_scene() {
        let frame = MirrorFrame {
            interaction_revision: None,
            private_scene: None,
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
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
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
