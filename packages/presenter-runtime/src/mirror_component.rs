//! Borrowed validated mirror in the canonical BMUX Component lifecycle.
use crate::{
    HostError,
    mirror::{CanvasPlacement, MirrorFrame},
};
use bmux_tui::{
    component::{
        Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode,
        LogicalSize,
    },
    event::{Event, EventOutcome},
    paint::{LocalRect, PaintCx},
};

pub struct MirrorComponent<'a> {
    frame: &'a MirrorFrame,
    max_cells: usize,
    max_bytes: usize,
}
impl<'a> MirrorComponent<'a> {
    pub fn new(
        frame: &'a MirrorFrame,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<Self, HostError> {
        frame.validate(max_cells, max_bytes)?;
        Ok(Self {
            frame,
            max_cells,
            max_bytes,
        })
    }
    /// Use the exact layout that was painted and successfully committed, never new
    /// viewport dimensions from an uncommitted resize. Coordinates are local to it.
    pub fn placement(&self, committed: &LayoutNode) -> Option<CanvasPlacement> {
        if committed.id != self.id() {
            return None;
        }
        Some(self.frame.placement(
            u16::try_from(committed.size.width).ok()?,
            u16::try_from(committed.size.height).ok()?,
        ))
    }
    pub fn input_point(&self, committed: &LayoutNode, x: u16, y: u16) -> Option<(u16, u16)> {
        self.placement(committed)?.input_point(x, y)
    }
    fn id(&self) -> LayoutId {
        LayoutId::new(format!("presentation/mirror/{}", self.frame.surface_id))
    }
}
impl Component for MirrorComponent<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            self.id(),
            constraints.constrain(LogicalSize::new(
                u64::from(self.frame.width),
                u64::from(self.frame.height),
            )),
        )
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(_placement) = self.placement(layout) else {
            return;
        };
        let Ok(width) = u16::try_from(layout.size.width) else {
            return;
        };
        let Ok(height) = u16::try_from(layout.size.height) else {
            return;
        };
        cx.with_child(0, 0, LocalRect::new(0, 0, width, height), |child| {
            let _validated =
                self.frame
                    .paint_viewport(child, width, height, self.max_cells, self.max_bytes);
        });
    }
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        // Application transports inert input with cursor/surface preconditions.
        EventOutcome::Ignored
    }
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            (u64::from(self.frame.width) << 16) | u64::from(self.frame.height),
            self.frame.revision,
        )
    }
}
