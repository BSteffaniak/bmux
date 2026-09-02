//! Panel, border, and modal chrome primitives.

use std::hash::{Hash, Hasher};

use crate::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use crate::frame::Frame;
use crate::geometry::{Insets, Rect, Size};
use crate::layout::centered;
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::Line;
use crate::text_block::Alignment;
use crate::text_width::display_width;

/// Border glyph set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSet {
    /// Top-left corner.
    pub top_left: char,
    /// Top-right corner.
    pub top_right: char,
    /// Bottom-left corner.
    pub bottom_left: char,
    /// Bottom-right corner.
    pub bottom_right: char,
    /// Horizontal edge.
    pub horizontal: char,
    /// Vertical edge.
    pub vertical: char,
}

impl BorderSet {
    /// Single-line border glyphs.
    pub const SINGLE: Self = Self {
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
        horizontal: '─',
        vertical: '│',
    };

    /// Rounded border glyphs.
    pub const ROUNDED: Self = Self {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: '─',
        vertical: '│',
    };

    /// ASCII-safe border glyphs.
    pub const ASCII: Self = Self {
        top_left: '+',
        top_right: '+',
        bottom_left: '+',
        bottom_right: '+',
        horizontal: '-',
        vertical: '|',
    };

    /// Double-line border glyphs.
    pub const DOUBLE: Self = Self {
        top_left: '╔',
        top_right: '╗',
        bottom_left: '╚',
        bottom_right: '╝',
        horizontal: '═',
        vertical: '║',
    };

    /// Thick border glyphs.
    pub const THICK: Self = Self {
        top_left: '┏',
        top_right: '┓',
        bottom_left: '┗',
        bottom_right: '┛',
        horizontal: '━',
        vertical: '┃',
    };
}

/// Border side selection.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSides {
    /// Render the top edge.
    pub top: bool,
    /// Render the right edge.
    pub right: bool,
    /// Render the bottom edge.
    pub bottom: bool,
    /// Render the left edge.
    pub left: bool,
}

impl BorderSides {
    /// All border sides.
    pub const ALL: Self = Self::new(true, true, true, true);
    /// No border sides.
    pub const NONE: Self = Self::new(false, false, false, false);
    /// Top border side only.
    pub const TOP: Self = Self::new(true, false, false, false);
    /// Right border side only.
    pub const RIGHT: Self = Self::new(false, true, false, false);
    /// Bottom border side only.
    pub const BOTTOM: Self = Self::new(false, false, true, false);
    /// Left border side only.
    pub const LEFT: Self = Self::new(false, false, false, true);

    /// Create a border side selection.
    #[allow(clippy::fn_params_excessive_bools)]
    #[must_use]
    pub const fn new(top: bool, right: bool, bottom: bool, left: bool) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Horizontal-only border sides.
    #[must_use]
    pub const fn horizontal() -> Self {
        Self::new(true, false, true, false)
    }

    /// Vertical-only border sides.
    #[must_use]
    pub const fn vertical() -> Self {
        Self::new(false, true, false, true)
    }

    /// Insets occupied by these border sides.
    #[must_use]
    pub const fn insets(self) -> Insets {
        Insets::new(
            if self.top { 1 } else { 0 },
            if self.right { 1 } else { 0 },
            if self.bottom { 1 } else { 0 },
            if self.left { 1 } else { 0 },
        )
    }
}

impl Default for BorderSides {
    fn default() -> Self {
        Self::ALL
    }
}

/// Border configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    /// Border glyphs.
    pub set: BorderSet,
    /// Border style.
    pub style: Style,
    /// Border sides to render.
    pub sides: BorderSides,
}

impl Border {
    /// Create a border with a glyph set and style.
    #[must_use]
    pub const fn new(set: BorderSet, style: Style) -> Self {
        Self {
            set,
            style,
            sides: BorderSides::ALL,
        }
    }

    /// Create a single-line border with default style.
    #[must_use]
    pub const fn single() -> Self {
        Self::new(BorderSet::SINGLE, Style::new())
    }

    /// Create a rounded border with default style.
    #[must_use]
    pub const fn rounded() -> Self {
        Self::new(BorderSet::ROUNDED, Style::new())
    }

    /// Create an ASCII-safe border with default style.
    #[must_use]
    pub const fn ascii() -> Self {
        Self::new(BorderSet::ASCII, Style::new())
    }

    /// Create a double-line border with default style.
    #[must_use]
    pub const fn double() -> Self {
        Self::new(BorderSet::DOUBLE, Style::new())
    }

    /// Create a thick border with default style.
    #[must_use]
    pub const fn thick() -> Self {
        Self::new(BorderSet::THICK, Style::new())
    }

    /// Set border sides.
    #[must_use]
    pub const fn sides(mut self, sides: BorderSides) -> Self {
        self.sides = sides;
        self
    }

    /// Set border style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Panel title position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitlePosition {
    /// Render title on the top border.
    #[default]
    Top,
    /// Render title on the bottom border.
    Bottom,
}

/// A panel title with position and alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelTitle {
    /// Title content.
    pub line: Line,
    /// Title position.
    pub position: TitlePosition,
    /// Horizontal title alignment.
    pub alignment: Alignment,
}

impl PanelTitle {
    /// Create a panel title.
    #[must_use]
    pub fn new(line: impl Into<Line>) -> Self {
        Self {
            line: line.into(),
            position: TitlePosition::Top,
            alignment: Alignment::Left,
        }
    }

    /// Set title position.
    #[must_use]
    pub const fn position(mut self, position: TitlePosition) -> Self {
        self.position = position;
        self
    }

    /// Set title alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }
}

impl From<Line> for PanelTitle {
    fn from(line: Line) -> Self {
        Self::new(line)
    }
}

impl From<&str> for PanelTitle {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PanelTitle {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// A rectangular panel with optional border, title, padding, and background.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    border: Option<Border>,
    title: Option<PanelTitle>,
    padding: Insets,
    background: Option<Style>,
    title_style: Option<Style>,
    content_style: Option<Style>,
}

impl Panel {
    /// Create an empty panel.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border: None,
            title: None,
            padding: Insets::new(0, 0, 0, 0),
            background: None,
            title_style: None,
            content_style: None,
        }
    }

    /// Set the panel border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// Set the panel title.
    #[must_use]
    pub fn title(mut self, title: impl Into<PanelTitle>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the panel padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Set the background style for the full panel area.
    #[must_use]
    pub const fn background(mut self, style: Style) -> Self {
        self.background = Some(style);
        self
    }

    /// Set the title fallback style.
    #[must_use]
    pub const fn title_style(mut self, style: Style) -> Self {
        self.title_style = Some(style);
        self
    }

    /// Set the content-area fallback style.
    #[must_use]
    pub const fn content_style(mut self, style: Style) -> Self {
        self.content_style = Some(style);
        self
    }

    /// Return the content area after border and padding are applied.
    #[must_use]
    pub const fn inner_area(&self, area: Rect) -> Rect {
        let border_insets = if let Some(border) = &self.border {
            border.sides.insets()
        } else {
            Insets::all(0)
        };
        area.inset(border_insets).inset(self.padding)
    }
}

impl Component for PanelComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        self.panel.padding.top.hash(&mut layout);
        self.panel.padding.right.hash(&mut layout);
        self.panel.padding.bottom.hash(&mut layout);
        self.panel.padding.left.hash(&mut layout);
        format!(
            "{:?}",
            self.panel.border.as_ref().map(|border| border.sides)
        )
        .hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.panel).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
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
        .with_metadata(LayoutMetadata::new().semantic("panel"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, layout.size.width, height);
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.panel.background {
            cx.fill(LocalRect::new(0, 0, area.width, area.height), " ", style);
        }
        if let Some(style) = self.panel.content_style {
            let inner = self.panel.inner_area(area);
            cx.fill(
                LocalRect::new(
                    i32::from(inner.x),
                    i64::from(inner.y),
                    inner.width,
                    inner.height,
                ),
                " ",
                style,
            );
        }
        if let Some(border) = &self.panel.border {
            paint_panel_border(area, border, cx);
            if let Some(title) = &self.panel.title {
                paint_panel_title(
                    area,
                    title,
                    self.panel.title_style.unwrap_or(border.style),
                    cx,
                );
            }
        }
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }
}

/// Canonical scoped-paint adapter for legacy child-owning containers.
pub struct PanelComponent<'a> {
    id: LayoutId,
    panel: &'a Panel,
}

impl<'a> PanelComponent<'a> {
    /// Borrow panel chrome under stable component identity.
    pub fn new(id: impl Into<LayoutId>, panel: &'a Panel) -> Self {
        Self {
            id: id.into(),
            panel,
        }
    }
}

fn paint_panel_border(area: Rect, border: &Border, cx: &mut PaintCx<'_, '_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let right = area.right().saturating_sub(1);
    let bottom = area.bottom().saturating_sub(1);
    let sides = border.sides;
    for (enabled, horizontal, coordinate) in [
        (sides.top, true, area.y),
        (sides.bottom && bottom != area.y, true, bottom),
        (sides.left, false, area.x),
        (sides.right && right != area.x, false, right),
    ] {
        if !enabled {
            continue;
        }
        if horizontal {
            for x in area.x..area.right() {
                cx.set_cell(
                    i32::from(x),
                    i64::from(coordinate),
                    &border.set.horizontal.to_string(),
                    border.style,
                );
            }
        } else {
            for y in area.y..area.bottom() {
                cx.set_cell(
                    i32::from(coordinate),
                    i64::from(y),
                    &border.set.vertical.to_string(),
                    border.style,
                );
            }
        }
    }
    if area.width > 1 && area.height > 1 {
        for (enabled, x, y, symbol) in [
            (sides.top && sides.left, area.x, area.y, border.set.top_left),
            (
                sides.top && sides.right,
                right,
                area.y,
                border.set.top_right,
            ),
            (
                sides.bottom && sides.left,
                area.x,
                bottom,
                border.set.bottom_left,
            ),
            (
                sides.bottom && sides.right,
                right,
                bottom,
                border.set.bottom_right,
            ),
        ] {
            if enabled {
                cx.set_cell(
                    i32::from(x),
                    i64::from(y),
                    &symbol.to_string(),
                    border.style,
                );
            }
        }
    }
}

fn paint_panel_title(area: Rect, title: &PanelTitle, style: Style, cx: &mut PaintCx<'_, '_>) {
    if area.width <= 2 || area.height == 0 {
        return;
    }
    let y = match title.position {
        TitlePosition::Top => area.y,
        TitlePosition::Bottom => area.bottom().saturating_sub(1),
    };
    let width = area.width.saturating_sub(2);
    let title_width = u16::try_from(display_width(&title.line.plain_text()))
        .unwrap_or(u16::MAX)
        .min(width);
    let offset = match title.alignment {
        Alignment::Left => 0,
        Alignment::Center => width.saturating_sub(title_width) / 2,
        Alignment::Right => width.saturating_sub(title_width),
    };
    cx.write_line(
        LocalRect::new(
            i32::from(area.x.saturating_add(1).saturating_add(offset)),
            i64::from(y),
            title_width,
            1,
        ),
        &title.line.with_fallback_style(style),
    );
}

/// A centered modal surface with optional scrim and child content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal<'widget, W> {
    panel: Panel,
    size: Size,
    scrim: Option<Style>,
    child: Option<&'widget W>,
}

impl<'widget, W> Modal<'widget, W> {
    /// Create a modal with the requested maximum size.
    #[must_use]
    pub const fn new(size: Size) -> Self {
        Self {
            panel: Panel::new().border(Border::single()),
            size,
            scrim: None,
            child: None,
        }
    }

    /// Set the panel used for modal chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set an optional full-area scrim style.
    #[must_use]
    pub const fn scrim(mut self, style: Style) -> Self {
        self.scrim = Some(style);
        self
    }

    /// Set modal child content.
    #[must_use]
    pub const fn child(mut self, child: &'widget W) -> Self {
        self.child = Some(child);
        self
    }

    /// Return the modal panel area for a parent area.
    #[must_use]
    pub const fn panel_area(&self, area: Rect) -> Rect {
        centered(area, self.size)
    }

    /// Return the modal content area for a parent area.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.panel.inner_area(self.panel_area(area))
    }
}

impl<W: Component> Modal<'_, W> {
    /// Paint this modal into an explicitly assigned terminal area.
    pub fn paint_in(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.scrim {
            frame.fill(area, " ", style);
        }
        let panel_area = self.panel_area(area);
        let panel = PanelComponent::new("modal.panel", &self.panel);
        let layout = panel.layout(Constraints::tight(panel_area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(panel_area.x),
            i64::from(panel_area.y),
            LocalRect::new(0, 0, panel_area.width, panel_area.height),
            |cx| panel.paint(&layout, cx),
        );
        if let Some(child) = self.child {
            let inner = self.panel.inner_area(panel_area);
            let layout = child.layout(Constraints::tight(inner.size()), &mut LayoutCx::new());
            PaintCx::new(frame).with_child(
                i32::from(inner.x),
                i64::from(inner.y),
                LocalRect::new(0, 0, inner.width, inner.height),
                |cx| child.paint(&layout, cx),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Border, BorderSides, Modal, Panel, PanelTitle, TitlePosition};
    use crate::buffer::Buffer;
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect, Size};
    use crate::paint::{LocalRect, PaintCx};
    use crate::style::{Color, Style};
    use crate::text_block::TextBlock;

    trait PanelTestRender {
        fn render(&self, area: Rect, frame: &mut Frame<'_>);
    }

    impl PanelTestRender for Panel {
        fn render(&self, area: Rect, frame: &mut Frame<'_>) {
            let component = super::PanelComponent::new("panel-test", self);
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            PaintCx::new(frame).with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
        }
    }

    #[test]
    fn modal_centers_panel_and_renders_child_in_inner_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        let child = TextBlock::new("Hi");
        let modal = Modal::new(Size::new(6, 3)).child(&child);

        modal.paint_in(Rect::new(0, 0, 10, 5), &mut frame);

        assert_eq!(
            modal.panel_area(Rect::new(0, 0, 10, 5)),
            Rect::new(2, 1, 6, 3)
        );
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  ┌────┐  "));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("  │Hi  │  "));
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("  └────┘  "));
    }

    #[test]
    fn modal_scrim_fills_parent_area_before_panel() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);
        let scrim = Style::new().bg(Color::BrightBlack);
        let panel = Panel::new().border(Border::ascii());
        let modal: Modal<'_, TextBlock> = Modal::new(Size::new(3, 3)).panel(panel).scrim(scrim);

        modal.paint_in(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" +-+ "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(scrim)
        );
    }

    #[test]
    fn panel_reports_inner_area() {
        let panel = Panel::new()
            .border(Border::single())
            .padding(Insets::new(1, 2, 3, 4));

        assert_eq!(
            panel.inner_area(Rect::new(0, 0, 20, 10)),
            Rect::new(5, 2, 12, 4)
        );
    }

    #[test]
    fn panel_renders_single_border() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::single())
            .render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("┌───┐"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("│   │"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("└───┘"));
    }

    #[test]
    fn panel_renders_title_over_top_border() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::ascii())
            .title("Title")
            .render(Rect::new(0, 0, 8, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("+Title-+"));
    }

    #[test]
    fn panel_renders_aligned_bottom_title() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::ascii())
            .title(
                PanelTitle::new("Title")
                    .position(TitlePosition::Bottom)
                    .alignment(crate::text_block::Alignment::Right),
            )
            .render(Rect::new(0, 0, 12, 3), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("+-----Title+")
        );
    }

    #[test]
    fn panel_renders_centered_title() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::ascii())
            .title(PanelTitle::new("Hi").alignment(crate::text_block::Alignment::Center))
            .render(Rect::new(0, 0, 11, 3), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("+---Hi----+")
        );
    }

    #[test]
    fn panel_inner_area_respects_selected_border_sides() {
        let panel = Panel::new().border(Border::single().sides(BorderSides::horizontal()));

        assert_eq!(
            panel.inner_area(Rect::new(0, 0, 10, 5)),
            Rect::new(0, 1, 10, 3)
        );
    }

    #[test]
    fn panel_renders_selected_border_sides() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::ascii().sides(BorderSides::horizontal()))
            .render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("-----"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("     "));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("-----"));
    }

    #[test]
    fn panel_renders_double_and_thick_borders() {
        let mut double_buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut double_frame = Frame::new(&mut double_buffer);
        Panel::new()
            .border(Border::double())
            .render(Rect::new(0, 0, 3, 2), &mut double_frame);
        assert_eq!(double_frame.buffer().row_symbols(0).as_deref(), Some("╔═╗"));

        let mut thick_buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut thick_frame = Frame::new(&mut thick_buffer);
        Panel::new()
            .border(Border::thick())
            .render(Rect::new(0, 0, 3, 2), &mut thick_frame);
        assert_eq!(thick_frame.buffer().row_symbols(0).as_deref(), Some("┏━┓"));
    }

    #[test]
    fn panel_renders_one_cell_borders_by_orientation() {
        let mut horizontal_buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut horizontal_frame = Frame::new(&mut horizontal_buffer);
        Panel::new()
            .border(Border::ascii().sides(BorderSides::TOP))
            .render(Rect::new(0, 0, 3, 1), &mut horizontal_frame);
        assert_eq!(
            horizontal_frame.buffer().row_symbols(0).as_deref(),
            Some("---")
        );

        let mut vertical_buffer = Buffer::empty(Rect::new(0, 0, 1, 3));
        let mut vertical_frame = Frame::new(&mut vertical_buffer);
        Panel::new()
            .border(Border::ascii().sides(BorderSides::LEFT))
            .render(Rect::new(0, 0, 1, 3), &mut vertical_frame);
        assert_eq!(vertical_frame.buffer().row_symbols(1).as_deref(), Some("|"));
    }

    #[test]
    fn panel_content_style_fills_inner_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 3));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().bg(Color::Blue);

        Panel::new()
            .border(Border::single())
            .content_style(style)
            .render(Rect::new(0, 0, 5, 3), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(2, 1))
                .map(|cell| cell.style),
            Some(style)
        );
        assert_ne!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn panel_title_style_overrides_border_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        let mut frame = Frame::new(&mut buffer);
        let border_style = Style::new().fg(Color::Red);
        let title_style = Style::new().fg(Color::Blue);

        Panel::new()
            .border(Border::ascii().style(border_style))
            .title("Title")
            .title_style(title_style)
            .render(Rect::new(0, 0, 8, 3), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(1, 0))
                .map(|cell| cell.style),
            Some(title_style)
        );
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(border_style)
        );
    }

    #[test]
    fn panel_zero_area_does_not_panic() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Panel::new()
            .border(Border::double())
            .title(PanelTitle::new("Hidden").position(TitlePosition::Bottom))
            .padding(Insets::all(1))
            .background(Style::new().bg(Color::Blue))
            .content_style(Style::new().bg(Color::Red))
            .render(Rect::new(0, 0, 0, 0), &mut frame);
    }

    #[test]
    fn panel_background_fills_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().bg(Color::Blue);

        Panel::new()
            .background(style)
            .render(Rect::new(0, 0, 4, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("    "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(style)
        );
    }
}
