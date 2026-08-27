//! Fundamental measurable composition containers.

use crate::chrome::Border;
use crate::component::{
    ChildLayout, Component, Constraints, Element, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use crate::geometry::Insets;
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::{Line, Text, TextWrap, TextWrapGeometry};
use crate::text_block::Alignment;

/// A measurable rich-text component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextContent {
    id: LayoutId,
    text: Text,
    style: Style,
    alignment: Alignment,
    wrap: TextWrap,
}

impl TextContent {
    /// Create rich text content.
    #[must_use]
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            id: LayoutId::new("text"),
            text: text.into(),
            style: Style::new(),
            alignment: Alignment::Left,
            wrap: TextWrap::Word,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set base text and row style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set text wrapping.
    #[must_use]
    pub const fn wrap(mut self, wrap: TextWrap) -> Self {
        self.wrap = wrap;
        self
    }

    /// Set horizontal alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    fn rows(&self, width: u16) -> Vec<Line> {
        let width = usize::from(width.max(1));
        self.text
            .lines
            .iter()
            .flat_map(|line| line.wrap(TextWrapGeometry::uniform(width), self.wrap))
            .collect()
    }
}

impl Component for TextContent {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(self.text.width())
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let rows = self.rows(width);
        let size = constraints.constrain(LogicalSize::new(width, rows.len()));
        LayoutNode::leaf(self.id.clone(), size)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        for (index, line) in self
            .rows(layout.size.width)
            .iter()
            .take(usize::from(height))
            .enumerate()
        {
            let line_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
            let x = match self.alignment {
                Alignment::Left => 0,
                Alignment::Center => layout.size.width.saturating_sub(line_width) / 2,
                Alignment::Right => layout.size.width.saturating_sub(line_width),
            };
            let row = u16::try_from(index).unwrap_or(u16::MAX);
            cx.write_line_with_fallback_style(
                LocalRect::new(0, i64::from(row), layout.size.width, 1),
                &Line::from_spans(
                    std::iter::once(crate::text::Span::raw(" ".repeat(usize::from(x))))
                        .chain(line.spans.iter().cloned())
                        .collect::<Vec<_>>(),
                ),
                self.style,
            );
        }
    }
}

/// A child-owning rectangular style, border, and padding container.
pub struct Surface<'a> {
    id: LayoutId,
    child: Element<'a>,
    background: Style,
    content_style: Style,
    border: Option<Border>,
    padding: Insets,
}

impl<'a> Surface<'a> {
    /// Create a surface containing one child.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("surface"),
            child: Element::new(child),
            background: Style::new(),
            content_style: Style::new(),
            border: None,
            padding: Insets::all(0),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set complete rectangular background style.
    #[must_use]
    pub const fn background(mut self, style: Style) -> Self {
        self.background = style;
        self
    }

    /// Set inherited child content style.
    #[must_use]
    pub const fn content_style(mut self, style: Style) -> Self {
        self.content_style = style;
        self
    }

    /// Set border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// Set child padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    const fn insets(&self) -> Insets {
        let border = match &self.border {
            Some(border) => border.sides.insets(),
            None => Insets::all(0),
        };
        Insets::new(
            border.top.saturating_add(self.padding.top),
            border.right.saturating_add(self.padding.right),
            border.bottom.saturating_add(self.padding.bottom),
            border.left.saturating_add(self.padding.left),
        )
    }
}

impl Component for Surface<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let insets = self.insets();
        let child = self.child.layout(
            constraints.inset(insets.horizontal(), usize::from(insets.vertical())),
            cx,
        );
        let size = constraints.constrain(LogicalSize::new(
            child.size.width.saturating_add(insets.horizontal()),
            child
                .size
                .height
                .saturating_add(usize::from(insets.vertical())),
        ));
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(
                insets.left,
                usize::from(insets.top),
                child,
            )],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        cx.fill(
            LocalRect::new(0, 0, layout.size.width, height),
            " ",
            self.background,
        );
        if let Some(border) = &self.border {
            paint_border(layout.size.width, height, border, cx);
        }
        let Some(child_layout) = layout.children.first() else {
            return;
        };
        let clip_height = u16::try_from(child_layout.node.size.height).unwrap_or(u16::MAX);
        cx.with_style(self.content_style, |cx| {
            cx.with_child(
                i32::from(child_layout.x),
                i64::try_from(child_layout.y).unwrap_or(i64::MAX),
                LocalRect::new(0, 0, child_layout.node.size.width, clip_height),
                |cx| self.child.paint(&child_layout.node, cx),
            );
        });
    }
}

fn paint_border(width: u16, height: u16, border: &Border, cx: &mut PaintCx<'_, '_>) {
    if width == 0 || height == 0 {
        return;
    }
    let right = width.saturating_sub(1);
    let bottom = height.saturating_sub(1);
    let sides = border.sides;
    if sides.top {
        for x in 0..width {
            cx.set_cell(
                i32::from(x),
                0,
                &border.set.horizontal.to_string(),
                border.style,
            );
        }
    }
    if sides.bottom && bottom != 0 {
        for x in 0..width {
            cx.set_cell(
                i32::from(x),
                i64::from(bottom),
                &border.set.horizontal.to_string(),
                border.style,
            );
        }
    }
    if sides.left {
        for y in 0..height {
            cx.set_cell(
                0,
                i64::from(y),
                &border.set.vertical.to_string(),
                border.style,
            );
        }
    }
    if sides.right && right != 0 {
        for y in 0..height {
            cx.set_cell(
                i32::from(right),
                i64::from(y),
                &border.set.vertical.to_string(),
                border.style,
            );
        }
    }
    if width > 1 && height > 1 {
        if sides.top && sides.left {
            cx.set_cell(0, 0, &border.set.top_left.to_string(), border.style);
        }
        if sides.top && sides.right {
            cx.set_cell(
                i32::from(right),
                0,
                &border.set.top_right.to_string(),
                border.style,
            );
        }
        if sides.bottom && sides.left {
            cx.set_cell(
                0,
                i64::from(bottom),
                &border.set.bottom_left.to_string(),
                border.style,
            );
        }
        if sides.bottom && sides.right {
            cx.set_cell(
                i32::from(right),
                i64::from(bottom),
                &border.set.bottom_right.to_string(),
                border.style,
            );
        }
    }
}

/// A measured viewport over one arbitrary component subtree.
pub struct ScrollViewport<'a> {
    id: LayoutId,
    child: Element<'a>,
    vertical_offset: usize,
}

impl<'a> ScrollViewport<'a> {
    /// Create a vertical viewport over arbitrary content.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("scroll-viewport"),
            child: Element::new(child),
            vertical_offset: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set caller-owned logical vertical offset.
    #[must_use]
    pub const fn vertical_offset(mut self, vertical_offset: usize) -> Self {
        self.vertical_offset = vertical_offset;
        self
    }

    /// Return maximum logical vertical offset for a resolved viewport.
    #[must_use]
    pub fn max_vertical_offset(layout: &LayoutNode) -> usize {
        layout.children.first().map_or(0, |child| {
            child.node.size.height.saturating_sub(layout.size.height)
        })
    }
}

impl Component for ScrollViewport<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width();
        let child = self
            .child
            .layout(Constraints::new(width, width, 0, None), cx);
        let natural_height = child.size.height;
        let viewport_height = constraints.max_height().map_or(natural_height, |maximum| {
            natural_height.clamp(constraints.min_height(), maximum)
        });
        let size = constraints.constrain(LogicalSize::new(width, viewport_height));
        LayoutNode::with_children(self.id.clone(), size, vec![ChildLayout::new(0, 0, child)])
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(child) = layout.children.first() else {
            return;
        };
        let viewport_height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let offset = self.vertical_offset.min(Self::max_vertical_offset(layout));
        cx.with_child(
            0,
            -i64::try_from(offset).unwrap_or(i64::MAX),
            LocalRect::new(
                0,
                i64::try_from(offset).unwrap_or(i64::MAX),
                layout.size.width,
                viewport_height,
            ),
            |cx| self.child.paint(&child.node, cx),
        );
    }
}

/// One child in a horizontal row.
struct RowChild<'a> {
    component: Element<'a>,
    flex: u16,
}

/// Horizontal composition supporting intrinsic and proportionally flexible children.
pub struct Row<'a> {
    id: LayoutId,
    children: Vec<RowChild<'a>>,
    gap: u16,
}

impl<'a> Row<'a> {
    /// Create an empty row.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: LayoutId::new("row"),
            children: Vec::new(),
            gap: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set cells between children.
    #[must_use]
    pub const fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    /// Append an intrinsically sized child.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(RowChild {
            component: Element::new(child),
            flex: 0,
        });
        self
    }

    /// Append a child receiving a proportional share of remaining width.
    #[must_use]
    pub fn flex_child(mut self, weight: u16, child: impl Component + 'a) -> Self {
        self.children.push(RowChild {
            component: Element::new(child),
            flex: weight.max(1),
        });
        self
    }
}

impl Default for Row<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Row<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let gaps = self.gap.saturating_mul(
            u16::try_from(self.children.len().saturating_sub(1)).unwrap_or(u16::MAX),
        );
        let available = constraints.max_width().saturating_sub(gaps);
        let mut resolved: Vec<Option<LayoutNode>> = vec![None; self.children.len()];
        let mut intrinsic_width = 0u16;
        let mut flex_weight = 0u32;
        for (index, child) in self.children.iter().enumerate() {
            if child.flex == 0 {
                let node = child.component.layout(
                    Constraints::new(0, available, 0, constraints.max_height()),
                    cx,
                );
                intrinsic_width = intrinsic_width.saturating_add(node.size.width);
                resolved[index] = Some(node);
            } else {
                flex_weight = flex_weight.saturating_add(u32::from(child.flex));
            }
        }
        let remaining = available.saturating_sub(intrinsic_width);
        let mut assigned_flex = 0u16;
        let mut seen_weight = 0u32;
        for (index, child) in self.children.iter().enumerate() {
            if child.flex == 0 {
                continue;
            }
            seen_weight = seen_weight.saturating_add(u32::from(child.flex));
            let cumulative = u32::from(remaining)
                .saturating_mul(seen_weight)
                .checked_div(flex_weight.max(1))
                .unwrap_or(0);
            let cumulative = u16::try_from(cumulative).unwrap_or(u16::MAX);
            let width = cumulative.saturating_sub(assigned_flex);
            assigned_flex = cumulative;
            resolved[index] = Some(child.component.layout(
                Constraints::new(width, width, 0, constraints.max_height()),
                cx,
            ));
        }
        let mut x = 0u16;
        let mut height = 0usize;
        let mut children = Vec::with_capacity(self.children.len());
        for node in resolved.into_iter().flatten() {
            height = height.max(node.size.height);
            let width = node.size.width;
            children.push(ChildLayout::new(x, 0, node));
            x = x.saturating_add(width).saturating_add(self.gap);
        }
        if !children.is_empty() {
            x = x.saturating_sub(self.gap);
        }
        let size = constraints.constrain(LogicalSize::new(x, height));
        LayoutNode::with_children(self.id.clone(), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (child, resolved) in self.children.iter().zip(&layout.children) {
            let height = u16::try_from(resolved.node.size.height).unwrap_or(u16::MAX);
            cx.with_child(
                i32::from(resolved.x),
                i64::try_from(resolved.y).unwrap_or(i64::MAX),
                LocalRect::new(0, 0, resolved.node.size.width, height),
                |cx| child.component.paint(&resolved.node, cx),
            );
        }
    }
}

/// Vertical composition of variable-height children.
pub struct Column<'a> {
    id: LayoutId,
    children: Vec<Element<'a>>,
    gap: usize,
}

impl<'a> Column<'a> {
    /// Create an empty column.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: LayoutId::new("column"),
            children: Vec::new(),
            gap: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set logical rows between children.
    #[must_use]
    pub const fn gap(mut self, gap: usize) -> Self {
        self.gap = gap;
        self
    }

    /// Append a child.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(Element::new(child));
        self
    }
}

impl Default for Column<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Column<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let mut y = 0usize;
        let width = constraints.max_width();
        let mut children = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let node = child.layout(
                Constraints::new(width, width, 0, constraints.max_height()),
                cx,
            );
            children.push(ChildLayout::new(0, y, node));
            y = y
                .saturating_add(children.last().map_or(0, |child| child.node.size.height))
                .saturating_add(self.gap);
        }
        if !children.is_empty() {
            y = y.saturating_sub(self.gap);
        }
        let size = constraints.constrain(LogicalSize::new(width, y));
        LayoutNode::with_children(self.id.clone(), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (component, child) in self.children.iter().zip(&layout.children) {
            let height = u16::try_from(child.node.size.height).unwrap_or(u16::MAX);
            cx.with_child(
                i32::from(child.x),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                LocalRect::new(0, 0, child.node.size.width, height),
                |cx| component.paint(&child.node, cx),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Column, Row, ScrollViewport, Surface, TextContent};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::{Insets, Point, Rect};
    use crate::paint::PaintCx;
    use crate::style::{Color, Style};

    #[test]
    fn surface_measures_padding_and_paints_complete_rectangle() {
        let component = Surface::new(TextContent::new("hello"))
            .background(Style::new().bg(Color::Blue))
            .padding(Insets::new(1, 1, 1, 1));
        let mut layout_cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(10), &mut layout_cx);
        assert_eq!(layout.size.height, 3);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(9, 1))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Blue))
        );
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" hello    "));
    }

    #[test]
    fn scroll_viewport_translates_and_clips_arbitrary_composed_content() {
        let component = ScrollViewport::new(
            Column::new()
                .child(TextContent::new("first"))
                .child(TextContent::new("second"))
                .child(TextContent::new("third")),
        )
        .vertical_offset(1);
        let constraints = Constraints::new(8, 8, 2, Some(2));
        let mut layout_cx = LayoutCx::new();
        let layout = component.layout(constraints, &mut layout_cx);
        assert_eq!(layout.size.height, 2);
        assert_eq!(ScrollViewport::max_vertical_offset(&layout), 1);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("second  "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("third   "));
    }

    #[test]
    fn row_assigns_remaining_width_to_flexible_child() {
        let component = Row::new()
            .gap(1)
            .child(TextContent::new("tag"))
            .flex_child(1, TextContent::new("flexible"));
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(12), &mut cx);

        assert_eq!(layout.children[0].node.size.width, 3);
        assert_eq!(layout.children[1].x, 4);
        assert_eq!(layout.children[1].node.size.width, 8);
        assert_eq!(layout.size.width, 12);
    }

    #[test]
    fn column_places_variable_height_children_with_gap() {
        let component = Column::new()
            .gap(1)
            .child(TextContent::new("one"))
            .child(TextContent::new("two words wrapping"));
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(8), &mut cx);

        assert_eq!(layout.children[0].y, 0);
        assert_eq!(layout.children[1].y, 2);
        assert_eq!(layout.size.height, 5);
    }
}
