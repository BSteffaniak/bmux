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
    logical_offset: Option<(i32, i64)>,
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
            logical_offset: None,
            // Keep raster coordinates component-local so selection, cursor, and
            // event metadata retain their existing coordinate contract. Only the
            // visible allocation consumes cells, not the full logical extent.
            raster: Rect::new(offset.x, offset.y, viewport.width, viewport.height),
        })
    }

    /// Construct a viewport using scoped logical transforms rather than a
    /// component-coordinate raster. Metadata is returned in surface coordinates;
    /// the buffer remains viewport-local. Components must honor scoped geometry
    /// for both painting and event routing.
    #[must_use]
    pub fn with_logical_offset(
        layout: LayoutNode,
        viewport: Rect,
        offset_x: u64,
        offset_y: u64,
    ) -> Option<Self> {
        let x = i32::try_from(offset_x).ok()?;
        let y = i64::try_from(offset_y).ok()?;
        viewport.x.checked_add(viewport.width)?;
        viewport.y.checked_add(viewport.height)?;
        let mut result = Self::new(layout, viewport, Point::new(0, 0))?;
        result.logical_offset = Some((x, y));
        result.raster = viewport;
        Some(result)
    }

    #[must_use]
    pub fn paint(&self, component: &dyn Component) -> ComponentViewportPaint {
        let mut buffer = Buffer::empty(self.raster);
        let mut frame = Frame::new(&mut buffer);
        if let Some((x, y)) = self.logical_offset {
            PaintCx::new(&mut frame).with_child_size(
                i32::from(self.viewport.x) - x,
                i64::from(self.viewport.y) - y,
                self.layout.size,
                |cx| component.paint(&self.layout, cx),
            );
        } else {
            component.paint(&self.layout, &mut PaintCx::new(&mut frame));
        }
        let raster_origin = if self.logical_offset.is_some() {
            Point::new(self.viewport.x, self.viewport.y)
        } else {
            self.offset
        };
        let hits = frame
            .hits()
            .regions()
            .iter()
            .filter_map(|region| {
                let visible = region.area.intersection(Rect::new(
                    raster_origin.x,
                    raster_origin.y,
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
                        .saturating_add(visible.x.saturating_sub(raster_origin.x)),
                    self.viewport
                        .y
                        .saturating_add(visible.y.saturating_sub(raster_origin.y)),
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
            if self.logical_offset.is_none() {
                cursor.position = self.project(cursor.position)?;
            }
            Some(cursor)
        });
        let mut visible = Buffer::empty(Rect::new(0, 0, self.viewport.width, self.viewport.height));
        for y in 0..self.viewport.height {
            for x in 0..self.viewport.width {
                let source = Point::new(
                    x.saturating_add(raster_origin.x),
                    y.saturating_add(raster_origin.y),
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
        if let Some((x, y)) = self.logical_offset {
            return EventCx::new(&self.layout).with_transform(
                0,
                0,
                i32::from(self.viewport.x) - x,
                i64::from(self.viewport.y) - y,
                self.viewport,
                |cx| component.event(event, &self.layout, cx),
            );
        }
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

    #[test]
    fn logical_viewport_paints_beyond_terminal_coordinates() {
        use bmux_tui::component::{Constraints, LayoutCx};
        let text = bmux_tui::composition::TextBlock::new(format!("{}end", "row\n".repeat(70_000)));
        let layout = text.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let viewport =
            ComponentViewport::with_logical_offset(layout, Rect::new(3, 4, 8, 1), 0, 70_000)
                .unwrap();
        let painted = viewport.paint(&text);
        assert_eq!(painted.buffer.cells().len(), 8);
        let row: String = painted
            .buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(row.trim_end(), "end");
    }

    #[test]
    fn logical_viewport_routes_events_through_the_paint_transform() {
        use bmux_tui::component::{Constraints, LayoutCx, LogicalRect};
        struct Target;
        impl Component for Target {
            fn layout(&self, _: Constraints, _: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf("target".into(), LogicalSize::new(100_000, 100_000))
            }
            fn paint(&self, _: &LayoutNode, _: &mut PaintCx<'_, '_>) {}
            fn event(&self, _: &Event, _: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
                assert_eq!(
                    cx.visible_rect(LogicalRect::new(70_002, 80_001, 2, 1)),
                    Rect::new(5, 5, 2, 1)
                );
                assert!(cx.visible_rect(LogicalRect::new(0, 0, 2, 1)).is_empty());
                EventOutcome::Redraw
            }
        }
        let target = Target;
        let layout = target.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let viewport =
            ComponentViewport::with_logical_offset(layout, Rect::new(3, 4, 8, 2), 70_000, 80_000)
                .unwrap();
        assert_eq!(viewport.event(&target, &Event::Tick), EventOutcome::Redraw);
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
