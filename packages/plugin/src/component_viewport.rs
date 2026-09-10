//! A bounded, reconstructible component viewport for retained plugin producers.
//!
//! State stays with the caller. Painting and event dispatch share one layout and
//! horizontal transform. Producers publish the returned cells through their existing
//! owner-scoped surface, and retain the viewport only after publication succeeds.
mod committed;
pub use committed::CommittedComponentViewport;

use bmux_tui::{
    buffer::Buffer,
    component::{Component, EventCx, LayoutNode},
    event::{Event, EventOutcome},
    frame::{Cursor, Frame},
    geometry::{Point, Rect},
    paint::PaintCx,
};

#[derive(Debug, Clone)]
pub struct ComponentViewport {
    layout: LayoutNode,
    viewport: Rect,
    offset: Point,
    raster: Rect,
}

pub struct ComponentViewportPaint {
    pub buffer: Buffer,
    /// Caller-owned hit geometry in surface coordinates, lowered with the same transform.
    /// Non-raster contributions in component-local coordinates. Consumers must
    /// explicitly lower these using the viewport transform or reject unsupported
    /// contributions; this raster bridge is not a general presentation transport.
    pub semantics: bmux_tui::semantic::SemanticScene,
    pub selection: bmux_tui::selection::SelectionScene,
    pub images: Vec<bmux_tui::image::ImageContribution>,
    pub hits: Vec<crate::surface::PluginSurfaceRegion>,
    pub cursor: Option<Cursor>,
}

impl ComponentViewport {
    /// A caller-resolved layout and its visible allocation in surface coordinates.
    ///
    /// Returns `None` for empty or oversized raster allocations.
    #[must_use]
    pub fn new(layout: LayoutNode, viewport: Rect, offset: Point) -> Option<Self> {
        if layout.size.width == 0
            || layout.size.height == 0
            || viewport.is_empty()
            || offset.x.checked_add(viewport.width).is_none()
            || offset.y.checked_add(viewport.height).is_none()
            || usize::from(viewport.width) * usize::from(viewport.height) > 65_536
        {
            return None;
        }
        Some(Self {
            layout,
            viewport,
            offset,
            // Keep raster coordinates component-local so selection, cursor, and
            // event metadata retain their existing coordinate contract. Only the
            // visible allocation consumes cells, not the full logical extent.
            raster: Rect::new(offset.x, offset.y, viewport.width, viewport.height),
        })
    }

    #[must_use]
    pub fn paint(&self, component: &dyn Component) -> ComponentViewportPaint {
        let mut buffer = Buffer::empty(self.raster);
        let mut frame = Frame::new(&mut buffer);
        component.paint(&self.layout, &mut PaintCx::new(&mut frame));
        let hits = frame
            .hits()
            .regions()
            .iter()
            .filter_map(|region| {
                let visible = region.area.intersection(Rect::new(
                    self.offset.x,
                    self.offset.y,
                    self.viewport.width,
                    self.viewport.height,
                ));
                if visible.is_empty() {
                    return None;
                }
                let mut region = region.clone();
                region.area = Rect::new(
                    self.viewport
                        .x
                        .saturating_add(visible.x.saturating_sub(self.offset.x)),
                    self.viewport
                        .y
                        .saturating_add(visible.y.saturating_sub(self.offset.y)),
                    visible.width,
                    visible.height,
                );
                Some(crate::surface::PluginSurfaceRegion::from_tui(&region))
            })
            .collect();
        let semantics = frame.semantics().clone();
        let selection = frame.selection().clone();
        let images = frame.images().to_vec();
        let cursor = frame.cursor().and_then(|mut cursor| {
            cursor.position = self.project(cursor.position)?;
            Some(cursor)
        });
        let mut visible = Buffer::empty(Rect::new(0, 0, self.viewport.width, self.viewport.height));
        for y in 0..self.viewport.height {
            for x in 0..self.viewport.width {
                let source = Point::new(
                    x.saturating_add(self.offset.x),
                    y.saturating_add(self.offset.y),
                );
                if let Some(cell) = buffer.get(source)
                    && let Some(target) = visible.get_mut(Point::new(x, y))
                {
                    *target = cell.clone();
                }
            }
        }
        ComponentViewportPaint {
            semantics,
            selection,
            images,
            hits,
            buffer: visible,
            cursor,
        }
    }

    /// Dispatch an already-routed event. Captured drags may be outside the viewport.
    pub fn event(&self, component: &dyn Component, event: &Event) -> EventOutcome {
        let mut event = event.clone();
        if let Event::Mouse(mouse) = &mut event {
            mouse.position.x =
                Self::unproject_axis(mouse.position.x, self.viewport.x, self.offset.x);
            mouse.position.y =
                Self::unproject_axis(mouse.position.y, self.viewport.y, self.offset.y);
        }
        component.event(&event, &self.layout, &mut EventCx::new(&self.layout))
    }

    /// Dispatch coordinates relative to the published surface allocation.
    pub fn event_local(&self, component: &dyn Component, event: &Event) -> EventOutcome {
        let mut event = event.clone();
        if let Event::Mouse(mouse) = &mut event {
            mouse.position.x = mouse.position.x.saturating_add(self.offset.x);
            mouse.position.y = mouse.position.y.saturating_add(self.offset.y);
        }
        component.event(&event, &self.layout, &mut EventCx::new(&self.layout))
    }

    pub fn hit_regions(
        &self,
        component: &dyn Component,
    ) -> Vec<crate::surface::PluginSurfaceRegion> {
        self.paint(component).hits
    }

    #[must_use]
    pub const fn visible_rect(&self) -> Rect {
        self.viewport
    }

    fn unproject_axis(position: u16, origin: u16, offset: u16) -> u16 {
        let logical = i32::from(position) - i32::from(origin) + i32::from(offset);
        u16::try_from(logical.clamp(0, i32::from(u16::MAX))).expect("clamped coordinate fits u16")
    }

    fn project(&self, point: Point) -> Option<Point> {
        let x = point.x.checked_sub(self.offset.x)?;
        let y = point.y.checked_sub(self.offset.y)?;
        (x < self.viewport.width && y < self.viewport.height).then_some(Point::new(
            self.viewport.x.saturating_add(x),
            self.viewport.y.saturating_add(y),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::component::{LayoutId, LogicalSize};

    #[test]
    fn logical_extent_does_not_determine_raster_allocation() {
        let viewport = ComponentViewport::new(
            LayoutNode::leaf("large".into(), LogicalSize::new(1_000_000, 1_000_000)),
            Rect::new(3, 4, 8, 2),
            Point::new(100, 200),
        )
        .unwrap();
        let buffer = Buffer::empty(viewport.raster);
        assert_eq!(buffer.cells().len(), 16);
        assert_eq!(buffer.area(), Rect::new(100, 200, 8, 2));
        assert_eq!(
            viewport.project(Point::new(102, 201)),
            Some(Point::new(5, 5))
        );
        assert_eq!(viewport.project(Point::new(99, 200)), None);
    }

    #[test]
    fn translated_input_does_not_saturate_before_subtracting_origin() {
        assert_eq!(
            ComponentViewport::unproject_axis(60_002, 60_000, 10_000),
            10_002
        );
        assert_eq!(ComponentViewport::unproject_axis(5, 10, 100), 95);
        assert_eq!(ComponentViewport::unproject_axis(0, 10, 2), 0);
        assert!(
            ComponentViewport::new(
                LayoutNode::leaf("overflow".into(), LogicalSize::new(100_000, 1)),
                Rect::new(0, 0, 10, 1),
                Point::new(65_530, 0),
            )
            .is_none()
        );
    }

    #[test]
    fn clipped_raster_preserves_component_coordinates() {
        use bmux_tui::component::{Constraints, LayoutCx};
        let text = bmux_tui::composition::TextBlock::new("zero\none\ntwo\nthree");
        let layout = text.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let viewport =
            ComponentViewport::new(layout, Rect::new(3, 4, 8, 1), Point::new(0, 2)).unwrap();
        let painted = viewport.paint(&text);
        let row: String = painted
            .buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(row.trim_end(), "two");
    }

    fn viewport(width: u64, height: u64) -> Option<ComponentViewport> {
        ComponentViewport::new(
            LayoutNode::leaf(
                LayoutId::new("raster-test"),
                LogicalSize::new(width, height),
            ),
            Rect::new(
                0,
                0,
                u16::try_from(width).ok()?,
                u16::try_from(height).ok()?,
            ),
            Point::new(0, 0),
        )
    }

    #[test]
    fn rejects_unrepresentable_or_oversized_rasters() {
        for (width, height) in [
            (65_536, 1),
            (1, 65_536),
            (u64::MAX, u64::MAX),
            (0, 1),
            (1, 0),
            (257, 256),
        ] {
            assert!(viewport(width, height).is_none());
        }
    }

    #[test]
    fn retains_exact_dimensions_at_allocation_limit() {
        let view = viewport(256, 256).expect("bounded raster");
        assert_eq!(view.raster, Rect::new(0, 0, 256, 256));
        assert!(viewport(65_535, 1).is_some());
    }
}
