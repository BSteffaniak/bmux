//! Canonical top-ordered overlay composition.

use std::hash::{Hash, Hasher};

use crate::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, LayoutCx, LayoutId,
    LayoutMetadata, LayoutNode, LogicalSize, combine_child_revisions,
};
use crate::geometry::Rect;
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;

/// One overlay layer placed over its parent surface.
pub struct OverlayLayer<'a> {
    area: Rect,
    clear_style: Option<Style>,
    component: Element<'a>,
}

impl<'a> OverlayLayer<'a> {
    /// Create an overlay layer for `component` in parent-local coordinates.
    #[must_use]
    pub fn new(area: Rect, component: impl Component + 'a) -> Self {
        Self {
            area,
            clear_style: None,
            component: Element::new(component),
        }
    }

    /// Set an optional clear/fill style for the layer area before painting.
    #[must_use]
    pub const fn clear_style(mut self, style: Style) -> Self {
        self.clear_style = Some(style);
        self
    }

    /// Return the parent-local overlay area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }
}

/// A stable-identity, top-ordered stack of overlay layers.
pub struct OverlayStack<'a> {
    id: LayoutId,
    layers: Vec<OverlayLayer<'a>>,
}

impl<'a> OverlayStack<'a> {
    /// Create an empty overlay stack.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>) -> Self {
        Self {
            id: id.into(),
            layers: Vec::new(),
        }
    }

    /// Push a layer above all existing layers.
    pub fn push(&mut self, layer: OverlayLayer<'a>) {
        self.layers.push(layer);
    }

    /// Return true when the stack has no layers.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Return layer count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.layers.len()
    }

    /// Return the top-most layer area, if any.
    #[must_use]
    pub fn top_area(&self) -> Option<Rect> {
        self.layers.last().map(OverlayLayer::area)
    }
}

impl Component for OverlayStack<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        for layer in &self.layers {
            layer.area.x.hash(&mut layout);
            layer.area.y.hash(&mut layout);
            layer.area.width.hash(&mut layout);
            layer.area.height.hash(&mut layout);
        }
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for layer in &self.layers {
            layer.clear_style.hash(&mut paint);
        }
        combine_child_revisions(
            ComponentRevision::new(layout.finish(), paint.finish()),
            self.layers.iter().map(|layer| layer.component.revision()),
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints
                .max_height()
                .unwrap_or_else(|| constraints.min_height()),
        ));
        let children = self
            .layers
            .iter()
            .map(|layer| {
                let width = layer
                    .area
                    .width
                    .min(size.width.saturating_sub(layer.area.x));
                let available_height = size.height.saturating_sub(usize::from(layer.area.y));
                let height = usize::from(layer.area.height).min(available_height);
                let node = layer
                    .component
                    .layout(Constraints::new(width, width, height, Some(height)), cx);
                ChildLayout::new(layer.area.x, usize::from(layer.area.y), node)
            })
            .collect();
        LayoutNode::with_children(self.id.clone(), size, children)
            .with_metadata(LayoutMetadata::new().semantic("overlay-stack"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (layer, child) in self.layers.iter().zip(&layout.children) {
            let height = u16::try_from(child.node.size.height).unwrap_or(u16::MAX);
            let clip = LocalRect::new(0, 0, child.node.size.width, height);
            cx.with_child(
                i32::from(child.x),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                clip,
                |cx| {
                    if let Some(style) = layer.clear_style {
                        cx.fill(clip, " ", style);
                        cx.push_damage(clip);
                    }
                    layer.component.paint(&child.node, cx);
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::composition::TextContent;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::paint::PaintCx;
    use crate::style::{Color, Style};

    use super::{OverlayLayer, OverlayStack};

    #[test]
    fn overlay_stack_paints_layers_in_order() {
        let bottom = TextContent::new("bottom").id("bottom");
        let top = TextContent::new("top").id("top");
        let mut stack = OverlayStack::new("overlays");
        stack.push(OverlayLayer::new(Rect::new(0, 0, 6, 1), bottom));
        stack.push(OverlayLayer::new(Rect::new(1, 0, 3, 1), top));
        let layout = stack.layout(
            Constraints::tight(Rect::new(0, 0, 6, 1).size()),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        stack.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("btopom"));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.top_area(), Some(Rect::new(1, 0, 3, 1)));
    }

    #[test]
    fn overlay_layer_can_clear_before_painting() {
        let clear = Style::new().bg(Color::Blue);
        let text = TextContent::new("x").id("text").style(clear);
        let mut stack = OverlayStack::new("overlays");
        stack.push(OverlayLayer::new(Rect::new(0, 0, 3, 1), text).clear_style(clear));
        let layout = stack.layout(
            Constraints::tight(Rect::new(0, 0, 3, 1).size()),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        stack.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("x  "));
        assert_eq!(
            frame.buffer().get(Point::new(1, 0)).map(|cell| cell.style),
            Some(clear)
        );
    }

    #[test]
    fn overlay_layout_clips_layers_to_parent_constraints() {
        let mut stack = OverlayStack::new("overlays");
        stack.push(OverlayLayer::new(
            Rect::new(3, 2, 9, 7),
            TextContent::new("outside").id("outside"),
        ));

        let layout = stack.layout(
            Constraints::tight(Rect::new(0, 0, 5, 4).size()),
            &mut LayoutCx::new(),
        );

        assert_eq!(layout.children[0].x, 3);
        assert_eq!(layout.children[0].y, 2);
        assert_eq!(layout.children[0].node.size.width, 2);
        assert_eq!(layout.children[0].node.size.height, 2);
    }
}
