//! Legacy product adapter over the neutral retained raster component.
use crate::{
    HostError,
    mirror::{CanvasPlacement, MirrorFrame},
};
use bmux_tui::{
    component::{Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode},
    embedded_surface::{EmbeddedSurface, RetainedSurface},
    event::{Event, EventOutcome},
    paint::PaintCx,
};

/// Decode once at ingestion, then borrow the native component during painting.
/// The lifetime retains source compatibility with the former borrowed adapter.
pub struct MirrorComponent<'a> {
    surface: RetainedSurface,
    source: std::marker::PhantomData<&'a MirrorFrame>,
}

impl<'a> MirrorComponent<'a> {
    pub fn new(
        frame: &'a MirrorFrame,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<Self, HostError> {
        Ok(Self {
            surface: frame.retained_surface(0, max_cells, max_bytes)?,
            source: std::marker::PhantomData,
        })
    }

    /// Pass only the layout successfully committed by the presentation owner.
    pub fn placement(&self, committed: &LayoutNode) -> Option<CanvasPlacement> {
        EmbeddedSurface::new(&self.surface).placement(committed)
    }

    pub fn input_point(&self, committed: &LayoutNode, x: u16, y: u16) -> Option<(u16, u16)> {
        let point = EmbeddedSurface::new(&self.surface).input_point(committed, x, y)?;
        Some((point.x, point.y))
    }
}

impl Component for MirrorComponent<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        EmbeddedSurface::new(&self.surface).layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        EmbeddedSurface::new(&self.surface).paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        EmbeddedSurface::new(&self.surface).event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        EmbeddedSurface::new(&self.surface).revision()
    }
}
