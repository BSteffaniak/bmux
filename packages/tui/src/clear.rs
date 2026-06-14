//! Explicit clear/fill widget.

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::Style;
use crate::widget::Widget;

/// A simple widget that clears/fills its render area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clear {
    style: Style,
}

impl Clear {
    /// Create a clear widget using the default style.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            style: Style::new(),
        }
    }

    /// Create a clear widget using `style`.
    #[must_use]
    pub const fn styled(style: Style) -> Self {
        Self { style }
    }

    /// Return this clear widget with a different style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl Widget for Clear {
    fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        frame.fill(area, " ", self.style);
    }
}

#[cfg(test)]
mod tests {
    use super::Clear;
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Style};
    use crate::widget::Widget;

    #[test]
    fn clear_blanks_area() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer.write_line(Rect::new(0, 0, 4, 1), &crate::text::Line::from("abcd"));
        let mut frame = Frame::new(&mut buffer);

        Clear::new().render(Rect::new(1, 0, 2, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("a  d"));
    }

    #[test]
    fn clear_applies_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().bg(Color::Blue);

        Clear::styled(style).render(Rect::new(0, 0, 2, 1), &mut frame);

        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Clear::new().render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
