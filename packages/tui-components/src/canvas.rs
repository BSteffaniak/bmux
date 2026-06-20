//! Lightweight deterministic canvas component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::style::Style;

/// Canvas coordinate bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasBounds {
    /// Minimum x value.
    pub x_min: f64,
    /// Maximum x value.
    pub x_max: f64,
    /// Minimum y value.
    pub y_min: f64,
    /// Maximum y value.
    pub y_max: f64,
}

impl CanvasBounds {
    /// Create canvas bounds.
    #[must_use]
    pub const fn new(x_min: f64, x_max: f64, y_min: f64, y_max: f64) -> Self {
        Self {
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }
}

/// One canvas point shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasPoint<'a> {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
    /// Marker symbol.
    pub marker: &'a str,
    /// Point style.
    pub style: Style,
}

impl<'a> CanvasPoint<'a> {
    /// Create a point with a marker.
    #[must_use]
    pub const fn new(x: f64, y: f64, marker: &'a str) -> Self {
        Self {
            x,
            y,
            marker,
            style: Style::new(),
        }
    }

    /// Return this point with style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// One canvas line shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasLine<'a> {
    /// Start x coordinate.
    pub x0: f64,
    /// Start y coordinate.
    pub y0: f64,
    /// End x coordinate.
    pub x1: f64,
    /// End y coordinate.
    pub y1: f64,
    /// Marker symbol.
    pub marker: &'a str,
    /// Line style.
    pub style: Style,
}

impl<'a> CanvasLine<'a> {
    /// Create a canvas line.
    #[must_use]
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64, marker: &'a str) -> Self {
        Self {
            x0,
            y0,
            x1,
            y1,
            marker,
            style: Style::new(),
        }
    }

    /// Return this line with style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Canvas rectangle rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasRectMode {
    /// Draw only rectangle edges.
    Outline,
    /// Fill the rectangle area.
    Fill,
}

/// One canvas rectangle shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect<'a> {
    /// Minimum x coordinate.
    pub x0: f64,
    /// Minimum y coordinate.
    pub y0: f64,
    /// Maximum x coordinate.
    pub x1: f64,
    /// Maximum y coordinate.
    pub y1: f64,
    /// Marker symbol.
    pub marker: &'a str,
    /// Rectangle style.
    pub style: Style,
    /// Rectangle rendering mode.
    pub mode: CanvasRectMode,
}

impl<'a> CanvasRect<'a> {
    /// Create a canvas rectangle outline.
    #[must_use]
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64, marker: &'a str) -> Self {
        Self {
            x0,
            y0,
            x1,
            y1,
            marker,
            style: Style::new(),
            mode: CanvasRectMode::Outline,
        }
    }

    /// Return this rectangle with style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Return this rectangle filled.
    #[must_use]
    pub const fn fill(mut self) -> Self {
        self.mode = CanvasRectMode::Fill;
        self
    }
}

/// One canvas circle shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasCircle<'a> {
    /// Center x coordinate.
    pub x: f64,
    /// Center y coordinate.
    pub y: f64,
    /// Radius in canvas coordinates.
    pub radius: f64,
    /// Marker symbol.
    pub marker: &'a str,
    /// Circle style.
    pub style: Style,
    /// Whether to fill the circle.
    pub filled: bool,
}

impl<'a> CanvasCircle<'a> {
    /// Create a canvas circle outline.
    #[must_use]
    pub const fn new(x: f64, y: f64, radius: f64, marker: &'a str) -> Self {
        Self {
            x,
            y,
            radius,
            marker,
            style: Style::new(),
            filled: false,
        }
    }

    /// Return this circle with style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Return this circle filled.
    #[must_use]
    pub const fn fill(mut self) -> Self {
        self.filled = true;
        self
    }
}

/// Deterministic canvas over caller-owned shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas<'a> {
    points: &'a [CanvasPoint<'a>],
    lines: &'a [CanvasLine<'a>],
    rects: &'a [CanvasRect<'a>],
    circles: &'a [CanvasCircle<'a>],
    bounds: CanvasBounds,
    style: Style,
}

impl<'a> Canvas<'a> {
    /// Create a canvas.
    #[must_use]
    pub const fn new(points: &'a [CanvasPoint<'a>], bounds: CanvasBounds) -> Self {
        Self {
            points,
            lines: &[],
            rects: &[],
            circles: &[],
            bounds,
            style: Style::new(),
        }
    }

    /// Return this canvas with fallback style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Return this canvas with line shapes.
    #[must_use]
    pub const fn lines(mut self, lines: &'a [CanvasLine<'a>]) -> Self {
        self.lines = lines;
        self
    }

    /// Return this canvas with rectangle shapes.
    #[must_use]
    pub const fn rects(mut self, rects: &'a [CanvasRect<'a>]) -> Self {
        self.rects = rects;
        self
    }

    /// Return this canvas with circle shapes.
    #[must_use]
    pub const fn circles(mut self, circles: &'a [CanvasCircle<'a>]) -> Self {
        self.circles = circles;
        self
    }

    /// Map a canvas point into an area cell.
    #[must_use]
    pub fn map_point(&self, area: Rect, x: f64, y: f64) -> Option<(u16, u16)> {
        map_point(area, self.bounds, x, y)
    }

    /// Render the canvas.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        for line in self.lines {
            if let (Some(start), Some(end)) = (
                self.map_point(area, line.x0, line.y0),
                self.map_point(area, line.x1, line.y1),
            ) {
                draw_line(
                    frame,
                    area,
                    start,
                    end,
                    line.marker,
                    style_or(line.style, self.style),
                );
            }
        }
        for rect in self.rects {
            self.draw_rect(area, frame, *rect);
        }
        for circle in self.circles {
            self.draw_circle(area, frame, *circle);
        }
        for point in self.points {
            if let Some((x, y)) = self.map_point(area, point.x, point.y) {
                let style = style_or(point.style, self.style);
                draw_cell(frame, area, x, y, point.marker, style);
            }
        }
    }
    fn draw_rect(&self, area: Rect, frame: &mut Frame<'_>, rect: CanvasRect<'_>) {
        let style = style_or(rect.style, self.style);
        if matches!(rect.mode, CanvasRectMode::Fill) {
            if let (Some(a), Some(b)) = (
                self.map_point(area, rect.x0, rect.y0),
                self.map_point(area, rect.x1, rect.y1),
            ) {
                let x_min = a.0.min(b.0);
                let x_max = a.0.max(b.0);
                let y_min = a.1.min(b.1);
                let y_max = a.1.max(b.1);
                for y in y_min..=y_max {
                    for x in x_min..=x_max {
                        draw_cell(frame, area, x, y, rect.marker, style);
                    }
                }
            }
            return;
        }
        let corners = [
            (rect.x0, rect.y0, rect.x1, rect.y0),
            (rect.x1, rect.y0, rect.x1, rect.y1),
            (rect.x1, rect.y1, rect.x0, rect.y1),
            (rect.x0, rect.y1, rect.x0, rect.y0),
        ];
        for (x0, y0, x1, y1) in corners {
            if let (Some(start), Some(end)) =
                (self.map_point(area, x0, y0), self.map_point(area, x1, y1))
            {
                draw_line(frame, area, start, end, rect.marker, style);
            }
        }
    }
    fn draw_circle(&self, area: Rect, frame: &mut Frame<'_>, circle: CanvasCircle<'_>) {
        if circle.radius <= 0.0 || area.is_empty() {
            return;
        }
        let style = style_or(circle.style, self.style);
        let x_step = (self.bounds.x_max - self.bounds.x_min) / f64::from(area.width.max(1));
        let y_step = (self.bounds.y_max - self.bounds.y_min) / f64::from(area.height.max(1));
        let tolerance = x_step.max(y_step).max(f64::EPSILON);
        for y in 0..area.height {
            for x in 0..area.width {
                let Some((canvas_x, canvas_y)) = cell_center(area, self.bounds, x, y) else {
                    continue;
                };
                let distance = (canvas_x - circle.x).hypot(canvas_y - circle.y);
                let should_draw = if circle.filled {
                    distance <= circle.radius
                } else {
                    (distance - circle.radius).abs() <= tolerance
                };
                if should_draw {
                    draw_cell(frame, area, x, y, circle.marker, style);
                }
            }
        }
    }
}

fn draw_cell(frame: &mut Frame<'_>, area: Rect, x: u16, y: u16, marker: &str, style: Style) {
    frame.buffer_mut().set_cell(
        Point::new(area.x.saturating_add(x), area.y.saturating_add(y)),
        marker,
        style,
    );
}

fn style_or(style: Style, fallback: Style) -> Style {
    if style == Style::new() {
        fallback
    } else {
        style
    }
}

fn draw_line(
    frame: &mut Frame<'_>,
    area: Rect,
    start: (u16, u16),
    end: (u16, u16),
    marker: &str,
    style: Style,
) {
    let x0 = i32::from(start.0);
    let y0 = i32::from(start.1);
    let x1 = i32::from(end.0);
    let y1 = i32::from(end.1);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    let mut x = x0;
    let mut y = y0;
    loop {
        if let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) {
            frame.buffer_mut().set_cell(
                Point::new(area.x.saturating_add(x), area.y.saturating_add(y)),
                marker,
                style,
            );
        }
        if x == x1 && y == y1 {
            break;
        }
        let double_error = error.saturating_mul(2);
        if double_error >= dy {
            error += dy;
            x += sx;
        }
        if double_error <= dx {
            error += dx;
            y += sy;
        }
    }
}

fn cell_center(area: Rect, bounds: CanvasBounds, x: u16, y: u16) -> Option<(f64, f64)> {
    if area.is_empty() || bounds.x_min >= bounds.x_max || bounds.y_min >= bounds.y_max {
        return None;
    }
    let x_ratio = if area.width <= 1 {
        0.5
    } else {
        f64::from(x) / f64::from(area.width.saturating_sub(1))
    };
    let y_ratio = if area.height <= 1 {
        0.5
    } else {
        f64::from(y) / f64::from(area.height.saturating_sub(1))
    };
    Some((
        bounds.x_min + x_ratio * (bounds.x_max - bounds.x_min),
        bounds.y_max - y_ratio * (bounds.y_max - bounds.y_min),
    ))
}

fn rounded_u16(value: f64) -> u16 {
    value
        .round()
        .clamp(0.0, f64::from(u16::MAX))
        .to_string()
        .parse::<u16>()
        .unwrap_or(0)
}

fn map_point(area: Rect, bounds: CanvasBounds, x: f64, y: f64) -> Option<(u16, u16)> {
    if area.is_empty()
        || bounds.x_min >= bounds.x_max
        || bounds.y_min >= bounds.y_max
        || x < bounds.x_min
        || x > bounds.x_max
        || y < bounds.y_min
        || y > bounds.y_max
    {
        return None;
    }
    let x_cell = (x - bounds.x_min) / (bounds.x_max - bounds.x_min)
        * f64::from(area.width.saturating_sub(1));
    let y_cell = (bounds.y_max - y) / (bounds.y_max - bounds.y_min)
        * f64::from(area.height.saturating_sub(1));
    Some((rounded_u16(x_cell), rounded_u16(y_cell)))
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::style::{Color, Style};

    use super::{Canvas, CanvasBounds, CanvasCircle, CanvasLine, CanvasPoint, CanvasRect};

    #[test]
    fn maps_canvas_coordinates_to_cells() {
        let points = [CanvasPoint::new(5.0, 5.0, "x")];
        let canvas = Canvas::new(&points, CanvasBounds::new(0.0, 10.0, 0.0, 10.0));

        assert_eq!(
            canvas.map_point(Rect::new(0, 0, 11, 11), 5.0, 5.0),
            Some((5, 5))
        );
    }

    #[test]
    fn renders_points_in_order() {
        let points = [
            CanvasPoint::new(0.0, 0.0, "a"),
            CanvasPoint::new(0.0, 0.0, "b").style(Style::new().fg(Color::Red)),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        let cell = frame.buffer().get(Point::new(0, 1));
        assert_eq!(cell.map(|cell| cell.symbol.as_str()), Some("b"));
        assert_eq!(cell.map(|cell| cell.style.fg), Some(Some(Color::Red)));
    }

    #[test]
    fn renders_lines_and_rectangles_before_points() {
        let points = [CanvasPoint::new(1.0, 1.0, "p")];
        let lines = [CanvasLine::new(0.0, 0.0, 2.0, 2.0, "l")];
        let rects = [CanvasRect::new(0.0, 0.0, 2.0, 2.0, "r")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 3));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 2.0, 0.0, 2.0))
            .lines(&lines)
            .rects(&rects)
            .render(Rect::new(0, 0, 3, 3), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(1, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("p")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 2))
                .map(|cell| cell.symbol.as_str()),
            Some("r")
        );
    }

    #[test]
    fn renders_filled_rectangles_and_circles_before_points() {
        let points = [CanvasPoint::new(2.0, 2.0, "p")];
        let rects = [CanvasRect::new(0.0, 0.0, 1.0, 1.0, "r").fill()];
        let circles = [CanvasCircle::new(2.0, 2.0, 1.0, "c").fill()];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 5));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 4.0, 0.0, 4.0))
            .rects(&rects)
            .circles(&circles)
            .render(Rect::new(0, 0, 5, 5), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 4))
                .map(|cell| cell.symbol.as_str()),
            Some("r")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("c")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 2))
                .map(|cell| cell.symbol.as_str()),
            Some("p")
        );
    }

    #[test]
    fn clips_out_of_bounds_and_handles_tiny_areas() {
        let points = [CanvasPoint::new(2.0, 2.0, "x")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);
        let canvas = Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0));

        assert_eq!(canvas.map_point(Rect::new(0, 0, 2, 2), 2.0, 2.0), None);
        canvas.render(Rect::new(0, 0, 0, 0), &mut frame);
    }
}
