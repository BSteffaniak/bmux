//! Panel, border, and modal chrome primitives.

use crate::frame::Frame;
use crate::geometry::{Insets, Rect, Size};
use crate::layout::centered;
use crate::style::Style;
use crate::text::Line;
use crate::widget::Widget;

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
}

/// Border configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    /// Border glyphs.
    pub set: BorderSet,
    /// Border style.
    pub style: Style,
}

impl Border {
    /// Create a border with a glyph set and style.
    #[must_use]
    pub const fn new(set: BorderSet, style: Style) -> Self {
        Self { set, style }
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

    /// Set border style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// A rectangular panel with optional border, title, padding, and background.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    border: Option<Border>,
    title: Option<Line>,
    padding: Insets,
    background: Option<Style>,
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
    pub fn title(mut self, title: impl Into<Line>) -> Self {
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

    /// Return the content area after border and padding are applied.
    #[must_use]
    pub const fn inner_area(&self, area: Rect) -> Rect {
        let border_insets = if self.border.is_some() {
            Insets::all(1)
        } else {
            Insets::all(0)
        };
        area.inset(border_insets).inset(self.padding)
    }
}

impl Widget for Panel {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.background {
            frame.fill(area, " ", style);
        }
        if let Some(border) = &self.border {
            render_border(area, border, frame);
            if let Some(title) = &self.title {
                render_title(area, title, border.style, frame);
            }
        }
    }
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

impl<W: Widget> Widget for Modal<'_, W> {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if let Some(style) = self.scrim {
            frame.fill(area, " ", style);
        }
        let panel_area = self.panel_area(area);
        self.panel.render(panel_area, frame);
        if let Some(child) = self.child {
            child.render(self.panel.inner_area(panel_area), frame);
        }
    }
}

fn render_border(area: Rect, border: &Border, frame: &mut Frame<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let right = area.right().saturating_sub(1);
    let bottom = area.bottom().saturating_sub(1);

    if area.height == 1 {
        for x in area.x..area.right() {
            frame.buffer_mut().set_cell(
                crate::geometry::Point::new(x, area.y),
                border.set.horizontal.to_string(),
                border.style,
            );
        }
        return;
    }

    if area.width == 1 {
        for y in area.y..area.bottom() {
            frame.buffer_mut().set_cell(
                crate::geometry::Point::new(area.x, y),
                border.set.vertical.to_string(),
                border.style,
            );
        }
        return;
    }

    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(area.x, area.y),
        border.set.top_left.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(right, area.y),
        border.set.top_right.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(area.x, bottom),
        border.set.bottom_left.to_string(),
        border.style,
    );
    frame.buffer_mut().set_cell(
        crate::geometry::Point::new(right, bottom),
        border.set.bottom_right.to_string(),
        border.style,
    );

    for x in area.x.saturating_add(1)..right {
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(x, area.y),
            border.set.horizontal.to_string(),
            border.style,
        );
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(x, bottom),
            border.set.horizontal.to_string(),
            border.style,
        );
    }

    for y in area.y.saturating_add(1)..bottom {
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(area.x, y),
            border.set.vertical.to_string(),
            border.style,
        );
        frame.buffer_mut().set_cell(
            crate::geometry::Point::new(right, y),
            border.set.vertical.to_string(),
            border.style,
        );
    }
}

fn render_title(area: Rect, title: &Line, style: Style, frame: &mut Frame<'_>) {
    if area.width <= 2 || area.height == 0 {
        return;
    }
    let title_area = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        1,
    );
    let styled_title = title.with_fallback_style(style);
    frame.write_line(title_area, &styled_title);
}

#[cfg(test)]
mod tests {
    use super::{Border, Modal, Panel};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect, Size};
    use crate::style::{Color, Style};
    use crate::text_block::TextBlock;
    use crate::widget::Widget;

    #[test]
    fn modal_centers_panel_and_renders_child_in_inner_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        let child = TextBlock::new("Hi");
        let modal = Modal::new(Size::new(6, 3)).child(&child);

        modal.render(Rect::new(0, 0, 10, 5), &mut frame);

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

        modal.render(Rect::new(0, 0, 5, 3), &mut frame);

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
