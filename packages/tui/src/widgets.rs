//! Built-in neutral widgets.

use crate::frame::Frame;
use crate::geometry::{Insets, Rect};
use crate::style::Style;
use crate::text::{Line, Text};
use crate::widget::Widget;

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    /// Align to the left edge.
    #[default]
    Left,
    /// Center within the available width.
    Center,
    /// Align to the right edge.
    Right,
}

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
    let styled_title = line_with_fallback_style(title, style);
    frame.write_line(title_area, &styled_title);
}

fn line_with_fallback_style(line: &Line, style: Style) -> Line {
    Line::from_spans(
        line.spans
            .iter()
            .map(|span| crate::text::Span::styled(span.content.clone(), style.patch(span.style)))
            .collect::<Vec<_>>(),
    )
}

/// A simple styled text block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    text: Text,
    alignment: Alignment,
}

impl TextBlock {
    /// Create a text block.
    #[must_use]
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            text: text.into(),
            alignment: Alignment::Left,
        }
    }

    /// Set horizontal alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Return this block's text.
    #[must_use]
    pub const fn text(&self) -> &Text {
        &self.text
    }
}

impl Widget for TextBlock {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }

        for (line_index, line) in self.text.lines.iter().enumerate() {
            let Ok(line_offset) = u16::try_from(line_index) else {
                return;
            };
            if line_offset >= area.height {
                return;
            }
            let line_area = aligned_line_area(area, line, self.alignment)
                .unwrap_or_else(|| Rect::new(area.x, area.y.saturating_add(line_offset), 0, 1));
            frame.write_line(
                Rect::new(
                    line_area.x,
                    area.y.saturating_add(line_offset),
                    line_area.width,
                    1,
                ),
                line,
            );
        }
    }
}

fn aligned_line_area(area: Rect, line: &Line, alignment: Alignment) -> Option<Rect> {
    let width = line_width(line).min(area.width);
    if width == 0 {
        return None;
    }
    let remaining = area.width.saturating_sub(width);
    let x_offset = match alignment {
        Alignment::Left => 0,
        Alignment::Center => remaining / 2,
        Alignment::Right => remaining,
    };
    Some(Rect::new(area.x.saturating_add(x_offset), area.y, width, 1))
}

fn line_width(line: &Line) -> u16 {
    let width: usize = line
        .spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_str()))
        .sum();
    u16::try_from(width).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Alignment, Border, Panel, TextBlock};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Insets, Rect};
    use crate::style::{Color, Style};
    use crate::text::{Line, Text};
    use crate::widget::Widget;

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

    #[test]
    fn text_block_renders_lines() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        let text = Text::from_lines(vec![Line::raw("hi"), Line::raw("yo")]);

        TextBlock::new(text).render(Rect::new(0, 0, 5, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("hi   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("yo   "));
    }

    #[test]
    fn text_block_honors_center_alignment() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        TextBlock::new("hi")
            .alignment(Alignment::Center)
            .render(Rect::new(0, 0, 6, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  hi  "));
    }
}
