//! Overlay stack primitives.

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::Style;
use crate::widget::Widget;

/// One overlay layer rendered over a base surface.
pub struct OverlayLayer<'widget> {
    area: Rect,
    clear_style: Option<Style>,
    widget: &'widget dyn Widget,
}

impl<'widget> OverlayLayer<'widget> {
    /// Create an overlay layer for `widget` in `area`.
    #[must_use]
    pub const fn new(area: Rect, widget: &'widget dyn Widget) -> Self {
        Self {
            area,
            clear_style: None,
            widget,
        }
    }

    /// Set an optional clear/fill style for the layer area before rendering.
    #[must_use]
    pub const fn clear_style(mut self, style: Style) -> Self {
        self.clear_style = Some(style);
        self
    }

    /// Return the overlay area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.area
    }
}

/// A top-ordered stack of overlay layers.
#[derive(Default)]
pub struct OverlayStack<'widget> {
    layers: Vec<OverlayLayer<'widget>>,
}

impl<'widget> OverlayStack<'widget> {
    /// Create an empty overlay stack.
    #[must_use]
    pub const fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Push a layer above all existing layers.
    pub fn push(&mut self, layer: OverlayLayer<'widget>) {
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

impl Widget for OverlayStack<'_> {
    fn render(&self, _area: Rect, frame: &mut Frame<'_>) {
        for layer in &self.layers {
            if let Some(style) = layer.clear_style {
                frame.fill(layer.area, " ", style);
            }
            layer.widget.render(layer.area, frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OverlayLayer, OverlayStack};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::style::{Color, Style};
    use crate::text_block::TextBlock;
    use crate::widget::Widget;

    #[test]
    fn overlay_stack_renders_layers_in_order() {
        let bottom = TextBlock::new("bottom");
        let top = TextBlock::new("top");
        let mut stack = OverlayStack::new();
        stack.push(OverlayLayer::new(Rect::new(0, 0, 6, 1), &bottom));
        stack.push(OverlayLayer::new(Rect::new(1, 0, 3, 1), &top));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        stack.render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("btopom"));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.top_area(), Some(Rect::new(1, 0, 3, 1)));
    }

    #[test]
    fn overlay_layer_can_clear_before_rendering() {
        let clear = Style::new().bg(Color::Blue);
        let text = TextBlock::new("x").style(clear);
        let mut stack = OverlayStack::new();
        stack.push(OverlayLayer::new(Rect::new(0, 0, 3, 1), &text).clear_style(clear));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);

        stack.render(Rect::new(0, 0, 3, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("x  "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(1, 0))
                .map(|cell| cell.style),
            Some(clear)
        );
    }
}
