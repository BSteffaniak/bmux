//! Explicit clear/fill component.

use std::hash::{Hash, Hasher};

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;

/// A component that clears or fills its resolved area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clear {
    id: LayoutId,
    style: Style,
}

impl Clear {
    /// Create a clear component using the default style.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>) -> Self {
        Self {
            id: id.into(),
            style: Style::new(),
        }
    }

    /// Create a styled clear component.
    #[must_use]
    pub fn styled(id: impl Into<LayoutId>, style: Style) -> Self {
        Self {
            id: id.into(),
            style,
        }
    }

    /// Return this component with a different style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl Component for Clear {
    fn revision(&self) -> ComponentRevision {
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.style.hash(&mut paint);
        ComponentRevision::new(0, paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints
                    .max_height()
                    .unwrap_or_else(|| constraints.min_height()),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("clear"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = LocalRect::new(0, 0, layout.size.width, height);
        cx.fill(area, " ", self.style);
        cx.push_damage(area);
    }
}

#[cfg(test)]
mod tests {
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::damage::{Damage, DamagePolicy};
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::paint::{LocalRect, PaintCx};
    use crate::style::{Color, Style};

    use super::Clear;

    #[test]
    fn clear_blanks_resolved_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer.write_line(Rect::new(0, 0, 4, 1), &crate::text::Line::from("abcd"));
        let component = Clear::new("clear");
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 2, 1).size()),
            &mut LayoutCx::new(),
        );
        let mut frame = Frame::new(&mut buffer);

        PaintCx::new(&mut frame).with_child(1, 0, LocalRect::new(0, 0, 2, 1), |cx| {
            component.paint(&layout, cx);
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("a  d"));
    }

    #[test]
    fn clear_applies_style_and_damage() {
        let style = Style::new().bg(Color::Blue);
        let component = Clear::styled("clear", style);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 2, 1).size()),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);

        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(style)
        );
        assert_ne!(frame.damage(DamagePolicy::default()), Damage::None);
    }

    #[test]
    fn zero_size_layout_does_not_paint() {
        let component = Clear::new("clear");
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 0, 0).size()),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.damage(DamagePolicy::default()), Damage::None);
    }
}
