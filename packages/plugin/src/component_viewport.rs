//! A bounded, reconstructible component viewport for retained plugin producers.
//!
//! State stays with the caller. Painting and event dispatch share one layout and
//! horizontal transform. Producers publish the returned cells through their existing
//! owner-scoped surface, and retain the viewport only after publication succeeds.
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
        let cells = usize::from(layout.size.width).checked_mul(layout.size.height)?;
        if cells == 0
            || cells > 65_536
            || viewport.is_empty()
            || usize::from(viewport.width) * usize::from(viewport.height) > 65_536
        {
            return None;
        }
        Some(Self {
            layout,
            viewport,
            offset,
        })
    }

    #[must_use]
    pub fn paint(&self, component: &dyn Component) -> ComponentViewportPaint {
        let height = u16::try_from(self.layout.size.height).unwrap_or(u16::MAX);
        let mut buffer = Buffer::empty(Rect::new(0, 0, self.layout.size.width, height));
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
            mouse.position.x = mouse
                .position
                .x
                .saturating_add(self.offset.x)
                .saturating_sub(self.viewport.x);
            mouse.position.y = mouse
                .position
                .y
                .saturating_add(self.offset.y)
                .saturating_sub(self.viewport.y);
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

    fn project(&self, point: Point) -> Option<Point> {
        let x = point.x.checked_sub(self.offset.x)?;
        let y = point.y.checked_sub(self.offset.y)?;
        (x < self.viewport.width && y < self.viewport.height).then_some(Point::new(
            self.viewport.x.saturating_add(x),
            self.viewport.y.saturating_add(y),
        ))
    }
}
