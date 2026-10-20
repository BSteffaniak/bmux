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
    focused_surface: Option<RetainedSurface>,
    cursor: Option<crate::mirror::MirrorCursor>,
    focused_cursor: Option<crate::mirror::MirrorCursor>,
    focused: bool,
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
            focused_surface: frame
                .focused
                .as_ref()
                .map(|focused| focused.retained_surface(0, max_cells, max_bytes))
                .transpose()?,
            cursor: frame.cursor,
            focused_cursor: frame.focused.as_ref().and_then(|focused| focused.cursor),
            focused: false,
            source: std::marker::PhantomData,
        })
    }

    /// Select focused paint using caller-local state, never shared mirror authority.
    #[must_use]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    fn surface(&self) -> &RetainedSurface {
        if self.focused {
            self.focused_surface.as_ref().unwrap_or(&self.surface)
        } else {
            &self.surface
        }
    }

    /// Pass only the layout successfully committed by the presentation owner.
    pub fn placement(&self, committed: &LayoutNode) -> Option<CanvasPlacement> {
        EmbeddedSurface::new(self.surface()).placement(committed)
    }

    pub fn input_point(&self, committed: &LayoutNode, x: u16, y: u16) -> Option<(u16, u16)> {
        let point = EmbeddedSurface::new(self.surface()).input_point(committed, x, y)?;
        Some((point.x, point.y))
    }
}

impl Component for MirrorComponent<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        EmbeddedSurface::new(self.surface()).layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        EmbeddedSurface::new(self.surface()).paint(layout, cx);
        let cursor = if self.focused {
            self.focused_cursor.or(self.cursor)
        } else {
            None
        };
        if let (Some(cursor), Some(placement)) = (cursor, self.placement(layout))
            && cursor.x >= placement.source_x
            && cursor.y >= placement.source_y
            && cursor.x < placement.source_x.saturating_add(placement.width)
            && cursor.y < placement.source_y.saturating_add(placement.height)
        {
            cx.set_cursor_local(
                placement.destination_x + cursor.x - placement.source_x,
                placement.destination_y + cursor.y - placement.source_y,
                cursor.visible,
            );
        }
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        EmbeddedSurface::new(self.surface()).event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        EmbeddedSurface::new(self.surface())
            .revision()
            .combine(ComponentRevision::new(0, u64::from(self.focused)))
    }
}
