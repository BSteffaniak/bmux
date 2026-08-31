//! Lightweight deterministic canvas component.
//!
//! Composition is intentionally simple and deterministic. Rasterized geometry is
//! applied in component order: lines, rectangles, circles, then points when
//! [`CanvasExplicitMarkers::Rasterize`] is selected. Overlapping rasterized
//! sub-cells keep the combined coverage mask, while the most recently written
//! shape style becomes the cell style. With the default
//! [`CanvasExplicitMarkers::Preserve`] policy, point markers are terminal-cell
//! overlays written after rasterized geometry, so the point marker and style win
//! for that cell.

use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
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

/// How explicit point markers are composed with rasterized geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasExplicitMarkers {
    /// Preserve point marker strings as terminal-cell overlays.
    Preserve,
    /// Rasterize points into the same sub-cell coverage buffer as geometry.
    Rasterize,
}

/// Glyph family preference used by the final per-cell compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasGlyphPreference {
    /// Pick the most compact exact glyph per cell, falling back to Braille.
    Auto,
    /// Prefer half-block glyphs when the cell can be represented exactly.
    PreferHalfBlock,
    /// Prefer quadrant glyphs when the cell can be represented exactly.
    PreferQuadrant,
    /// Prefer Braille for all non-explicit sub-cell coverage.
    PreferBraille,
}

/// Canvas rendering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasPolicy {
    /// Explicit point marker handling.
    pub explicit_markers: CanvasExplicitMarkers,
    /// Final glyph selection preference.
    pub glyph_preference: CanvasGlyphPreference,
}

impl CanvasPolicy {
    /// Automatic mixed-glyph canvas policy.
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            explicit_markers: CanvasExplicitMarkers::Preserve,
            glyph_preference: CanvasGlyphPreference::Auto,
        }
    }

    /// Rasterize point markers into coverage instead of preserving marker strings.
    #[must_use]
    pub const fn rasterized_points(mut self) -> Self {
        self.explicit_markers = CanvasExplicitMarkers::Rasterize;
        self
    }

    /// Prefer half-block glyphs when representable exactly.
    #[must_use]
    pub const fn prefer_half_block(mut self) -> Self {
        self.glyph_preference = CanvasGlyphPreference::PreferHalfBlock;
        self
    }

    /// Prefer quadrant glyphs when representable exactly.
    #[must_use]
    pub const fn prefer_quadrant(mut self) -> Self {
        self.glyph_preference = CanvasGlyphPreference::PreferQuadrant;
        self
    }

    /// Prefer Braille glyphs for sub-cell coverage.
    #[must_use]
    pub const fn prefer_braille(mut self) -> Self {
        self.glyph_preference = CanvasGlyphPreference::PreferBraille;
        self
    }
}

impl Default for CanvasPolicy {
    fn default() -> Self {
        Self::auto()
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
    /// Marker symbol retained for API compatibility; geometry is rasterized.
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
    /// Marker symbol retained for API compatibility; geometry is rasterized.
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
    /// Marker symbol retained for API compatibility; geometry is rasterized.
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
    policy: CanvasPolicy,
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
            policy: CanvasPolicy::auto(),
        }
    }

    /// Return this canvas with rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: CanvasPolicy) -> Self {
        self.policy = policy;
        self
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

    /// Render the canvas through one mixed-glyph raster/composition pipeline.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        let layout = self.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| self.paint(&layout, cx),
        );
    }

    fn raster(&self, area: Rect) -> Option<CanvasRaster<'a>> {
        let mut raster = CanvasRaster::new(area, self.bounds, self.policy)?;
        for line in self.lines {
            if let (Some(start), Some(end)) = (
                raster.map_point(line.x0, line.y0),
                raster.map_point(line.x1, line.y1),
            ) {
                raster.draw_line(start, end, style_or(line.style, self.style));
            }
        }
        for rect in self.rects {
            raster.draw_rect(*rect, style_or(rect.style, self.style));
        }
        for circle in self.circles {
            raster.draw_circle(*circle, style_or(circle.style, self.style));
        }
        for point in self.points {
            let style = style_or(point.style, self.style);
            match self.policy.explicit_markers {
                CanvasExplicitMarkers::Preserve => {
                    if let Some((dot_x, dot_y)) = raster.map_point(point.x, point.y) {
                        raster.set_symbol(dot_x / 2, dot_y / 4, point.marker, style);
                    }
                }
                CanvasExplicitMarkers::Rasterize => {
                    if let Some((dot_x, dot_y)) = raster.map_point(point.x, point.y) {
                        raster.set_dot(dot_x, dot_y, style);
                    }
                }
            }
        }
        Some(raster)
    }
}

impl Component for Canvas<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("canvas"),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or_default(),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = layout.size.width;
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let Some(raster) = self.raster(Rect::new(0, 0, width, height)) else {
            return;
        };
        cx.rasterize(LocalRect::new(0, 0, width, height), |x, y| {
            let x = u16::try_from(x).ok()?;
            let y = u16::try_from(y).ok()?;
            raster.output_cell(x, y)
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RasterCell<'a> {
    mask: u8,
    style: Style,
    symbol: Option<(&'a str, Style)>,
}

#[derive(Debug, Clone, PartialEq)]
struct CanvasRaster<'a> {
    area: Rect,
    bounds: CanvasBounds,
    policy: CanvasPolicy,
    dot_width: u16,
    dot_height: u16,
    cells: Vec<RasterCell<'a>>,
}

impl<'a> CanvasRaster<'a> {
    fn new(area: Rect, bounds: CanvasBounds, policy: CanvasPolicy) -> Option<Self> {
        if area.is_empty() || bounds.x_min >= bounds.x_max || bounds.y_min >= bounds.y_max {
            return None;
        }
        let cell_count = usize::from(area.width).checked_mul(usize::from(area.height))?;
        Some(Self {
            area,
            bounds,
            policy,
            dot_width: area.width.saturating_mul(2),
            dot_height: area.height.saturating_mul(4),
            cells: vec![
                RasterCell {
                    mask: 0,
                    style: Style::new(),
                    symbol: None,
                };
                cell_count
            ],
        })
    }

    fn map_point(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        if x < self.bounds.x_min
            || x > self.bounds.x_max
            || y < self.bounds.y_min
            || y > self.bounds.y_max
        {
            return None;
        }
        let x_span = self.bounds.x_max - self.bounds.x_min;
        let y_span = self.bounds.y_max - self.bounds.y_min;
        let x_scaled =
            (x - self.bounds.x_min) / x_span * f64::from(self.dot_width.saturating_sub(1));
        let y_scaled =
            (self.bounds.y_max - y) / y_span * f64::from(self.dot_height.saturating_sub(1));
        Some((
            rounded_u16(x_scaled).min(self.dot_width.saturating_sub(1)),
            rounded_u16(y_scaled).min(self.dot_height.saturating_sub(1)),
        ))
    }

    fn dot_center(&self, x: u16, y: u16) -> (f64, f64) {
        let x_ratio = if self.dot_width <= 1 {
            0.5
        } else {
            f64::from(x) / f64::from(self.dot_width.saturating_sub(1))
        };
        let y_ratio = if self.dot_height <= 1 {
            0.5
        } else {
            f64::from(y) / f64::from(self.dot_height.saturating_sub(1))
        };
        (
            self.bounds.x_min + x_ratio * (self.bounds.x_max - self.bounds.x_min),
            self.bounds.y_max - y_ratio * (self.bounds.y_max - self.bounds.y_min),
        )
    }

    fn cell_index(&self, cell_x: u16, cell_y: u16) -> Option<usize> {
        if cell_x >= self.area.width || cell_y >= self.area.height {
            return None;
        }
        Some(
            usize::from(cell_y)
                .saturating_mul(usize::from(self.area.width))
                .saturating_add(usize::from(cell_x)),
        )
    }

    fn set_symbol(&mut self, cell_x: u16, cell_y: u16, symbol: &'a str, style: Style) {
        let Some(index) = self.cell_index(cell_x, cell_y) else {
            return;
        };
        if let Some(cell) = self.cells.get_mut(index) {
            cell.symbol = Some((symbol, style));
        }
    }

    fn set_dot(&mut self, x: u16, y: u16, style: Style) {
        if x >= self.dot_width || y >= self.dot_height {
            return;
        }
        let cell_x = x / 2;
        let cell_y = y / 4;
        let Some(index) = self.cell_index(cell_x, cell_y) else {
            return;
        };
        if let Some(cell) = self.cells.get_mut(index) {
            cell.mask |= braille_dot_mask(x % 2, y % 4);
            cell.style = style;
        }
    }

    fn draw_line(&mut self, start: (u16, u16), end: (u16, u16), style: Style) {
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
                self.set_dot(x, y, style);
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

    fn draw_rect(&mut self, rect: CanvasRect<'_>, style: Style) {
        let (Some(a), Some(b)) = (
            self.map_point(rect.x0, rect.y0),
            self.map_point(rect.x1, rect.y1),
        ) else {
            return;
        };
        let x_min = a.0.min(b.0);
        let x_max = a.0.max(b.0);
        let y_min = a.1.min(b.1);
        let y_max = a.1.max(b.1);
        if matches!(rect.mode, CanvasRectMode::Fill) {
            for y in y_min..=y_max {
                for x in x_min..=x_max {
                    self.set_dot(x, y, style);
                }
            }
        } else {
            self.draw_line((x_min, y_min), (x_max, y_min), style);
            self.draw_line((x_max, y_min), (x_max, y_max), style);
            self.draw_line((x_max, y_max), (x_min, y_max), style);
            self.draw_line((x_min, y_max), (x_min, y_min), style);
        }
    }

    fn draw_circle(&mut self, circle: CanvasCircle<'_>, style: Style) {
        if circle.radius <= 0.0 {
            return;
        }
        let x_step = (self.bounds.x_max - self.bounds.x_min) / f64::from(self.dot_width.max(1));
        let y_step = (self.bounds.y_max - self.bounds.y_min) / f64::from(self.dot_height.max(1));
        let tolerance = x_step.max(y_step).max(f64::EPSILON);
        for y in 0..self.dot_height {
            for x in 0..self.dot_width {
                let (canvas_x, canvas_y) = self.dot_center(x, y);
                let distance = (canvas_x - circle.x).hypot(canvas_y - circle.y);
                let should_draw = if circle.filled {
                    distance <= circle.radius
                } else {
                    (distance - circle.radius).abs() <= tolerance
                };
                if should_draw {
                    self.set_dot(x, y, style);
                }
            }
        }
    }

    fn output_cell(&self, cell_x: u16, cell_y: u16) -> Option<(String, Style)> {
        let cell = self.cells.get(self.cell_index(cell_x, cell_y)?)?;
        if let Some((symbol, style)) = cell.symbol {
            Some((symbol.to_owned(), style))
        } else if cell.mask == 0 {
            None
        } else {
            Some((
                compose_mask(cell.mask, self.policy.glyph_preference).to_string(),
                cell.style,
            ))
        }
    }
}

fn style_or(style: Style, fallback: Style) -> Style {
    if style == Style::new() {
        fallback
    } else {
        style
    }
}

fn map_point(area: Rect, bounds: CanvasBounds, x: f64, y: f64) -> Option<(u16, u16)> {
    if area.is_empty() || bounds.x_min >= bounds.x_max || bounds.y_min >= bounds.y_max {
        return None;
    }
    if x < bounds.x_min || x > bounds.x_max || y < bounds.y_min || y > bounds.y_max {
        return None;
    }
    let x_span = bounds.x_max - bounds.x_min;
    let y_span = bounds.y_max - bounds.y_min;
    let x_scaled = (x - bounds.x_min) / x_span * f64::from(area.width.saturating_sub(1));
    let y_scaled = (bounds.y_max - y) / y_span * f64::from(area.height.saturating_sub(1));
    Some((
        rounded_u16(x_scaled).min(area.width.saturating_sub(1)),
        rounded_u16(y_scaled).min(area.height.saturating_sub(1)),
    ))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rounded_u16(value: f64) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= f64::from(u16::MAX) {
        u16::MAX
    } else {
        value.round() as u16
    }
}

fn compose_mask(mask: u8, preference: CanvasGlyphPreference) -> char {
    let half = half_block_from_braille(mask);
    let quadrant = quadrant_from_braille(mask);
    match preference {
        CanvasGlyphPreference::PreferBraille => braille_char(mask),
        CanvasGlyphPreference::PreferHalfBlock | CanvasGlyphPreference::Auto => {
            half.or(quadrant).unwrap_or_else(|| braille_char(mask))
        }
        CanvasGlyphPreference::PreferQuadrant => {
            quadrant.or(half).unwrap_or_else(|| braille_char(mask))
        }
    }
}

const fn half_block_from_braille(mask: u8) -> Option<char> {
    let top = mask & 0x3f;
    let bottom = mask & 0xc0;
    match (top, bottom) {
        (0x3f, 0x00) => Some('▀'),
        (0x00, 0xc0) => Some('▄'),
        (0x3f, 0xc0) => Some('█'),
        _ => None,
    }
}

const fn quadrant_from_braille(mask: u8) -> Option<char> {
    let tl = mask & 0x07;
    let tr = mask & 0x38;
    let bl = mask & 0x40;
    let br = mask & 0x80;
    let mut q = 0u8;
    if tl == 0x07 {
        q |= 0x01;
    } else if tl != 0 {
        return None;
    }
    if tr == 0x38 {
        q |= 0x02;
    } else if tr != 0 {
        return None;
    }
    if bl == 0x40 {
        q |= 0x04;
    }
    if br == 0x80 {
        q |= 0x08;
    }
    Some(quadrant_char(q))
}

const fn quadrant_char(mask: u8) -> char {
    match mask & 0x0f {
        0x01 => '▘',
        0x02 => '▝',
        0x03 => '▀',
        0x04 => '▖',
        0x05 => '▌',
        0x06 => '▞',
        0x07 => '▛',
        0x08 => '▗',
        0x09 => '▚',
        0x0a => '▐',
        0x0b => '▜',
        0x0c => '▄',
        0x0d => '▙',
        0x0e => '▟',
        0x0f => '█',
        _ => ' ',
    }
}

const fn braille_dot_mask(x: u16, y: u16) -> u8 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

fn braille_char(mask: u8) -> char {
    char::from_u32(0x2800 + u32::from(mask)).unwrap_or('\u{2800}')
}

#[cfg(test)]
mod tests {
    use super::{
        Canvas, CanvasBounds, CanvasCircle, CanvasExplicitMarkers, CanvasLine, CanvasPoint,
        CanvasPolicy, CanvasRect,
    };
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::style::{Color, Style};

    #[test]
    fn component_paint_clips_raster_output_to_the_scoped_viewport() {
        let points = [CanvasPoint::new(1.0, 0.0, "x")];
        let canvas = Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0));
        let layout = canvas.layout(
            Constraints::tight(bmux_tui::geometry::Size::new(2, 2)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut frame = Frame::new(&mut buffer);

        PaintCx::new(&mut frame).with_child(1, 0, LocalRect::new(0, 0, 1, 2), |cx| {
            canvas.paint(&layout, cx);
        });

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(1, 1))
                .map(|cell| cell.symbol.as_str()),
            Some(" ")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 1))
                .map(|cell| cell.symbol.as_str()),
            Some(" ")
        );
    }

    #[test]
    fn maps_canvas_coordinates_to_cells() {
        let canvas = Canvas::new(&[], CanvasBounds::new(0.0, 10.0, 0.0, 10.0));

        assert_eq!(
            canvas.map_point(Rect::new(0, 0, 11, 11), 0.0, 10.0),
            Some((0, 0))
        );
        assert_eq!(
            canvas.map_point(Rect::new(0, 0, 11, 11), 10.0, 0.0),
            Some((10, 10))
        );
        assert_eq!(canvas.map_point(Rect::new(0, 0, 11, 11), -1.0, 0.0), None);
    }

    #[test]
    fn rasterized_overlaps_use_last_shape_style_and_combined_mask() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let lines = [
            CanvasLine::new(0.0, 1.0, 0.0, 1.0, "ignored").style(red),
            CanvasLine::new(1.0, 1.0, 1.0, 1.0, "ignored").style(blue),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .lines(&lines)
            .policy(CanvasPolicy::auto().rasterized_points())
            .render(Rect::new(0, 0, 1, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("⠉"));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(blue)
        );
    }

    #[test]
    fn explicit_point_marker_overlays_rasterized_geometry() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let points = [CanvasPoint::new(0.0, 1.0, "p").style(blue)];
        let rects = [CanvasRect::new(0.0, 1.0, 0.0, 1.0, "ignored").style(red)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .rects(&rects)
            .render(Rect::new(0, 0, 1, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("p"));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(blue)
        );
    }

    #[test]
    fn preserves_point_markers_by_default() {
        let points = [
            CanvasPoint::new(0.0, 1.0, "●"),
            CanvasPoint::new(1.0, 0.0, "◆"),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .render(Rect::new(0, 0, 2, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("●◆"));
    }

    #[test]
    fn auto_composes_full_half_quadrant_and_braille_cells() {
        let full = [CanvasRect::new(0.0, 0.0, 1.0, 1.0, "ignored").fill()];
        let mut full_buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut full_frame = Frame::new(&mut full_buffer);
        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .rects(&full)
            .policy(CanvasPolicy::auto().rasterized_points())
            .render(Rect::new(0, 0, 1, 1), &mut full_frame);
        assert_eq!(full_frame.buffer().row_symbols(0).as_deref(), Some("█"));

        let top = [CanvasRect::new(0.0, 0.5, 1.0, 1.0, "ignored").fill()];
        let mut top_buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut top_frame = Frame::new(&mut top_buffer);
        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .rects(&top)
            .render(Rect::new(0, 0, 1, 1), &mut top_frame);
        assert_eq!(top_frame.buffer().row_symbols(0).as_deref(), Some("▀"));

        let left = [CanvasRect::new(0.0, 0.0, 0.0, 1.0, "ignored")];
        let mut left_buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut left_frame = Frame::new(&mut left_buffer);
        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .rects(&left)
            .render(Rect::new(0, 0, 1, 1), &mut left_frame);
        assert_eq!(left_frame.buffer().row_symbols(0).as_deref(), Some("▌"));

        let diagonal = [CanvasLine::new(0.0, 0.0, 1.0, 1.0, "ignored")];
        let mut diagonal_buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut diagonal_frame = Frame::new(&mut diagonal_buffer);
        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .lines(&diagonal)
            .render(Rect::new(0, 0, 1, 1), &mut diagonal_frame);
        assert_eq!(diagonal_frame.buffer().row_symbols(0).as_deref(), Some("⡜"));
    }

    #[test]
    fn preferences_use_same_raster_pipeline() {
        let diagonal = [CanvasLine::new(0.0, 0.0, 1.0, 1.0, "ignored")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&[], CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .lines(&diagonal)
            .policy(CanvasPolicy::auto().prefer_braille())
            .render(Rect::new(0, 0, 1, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("⡜"));
    }

    #[test]
    fn rasterized_points_share_coverage_and_style() {
        let point_style = Style::new().fg(Color::Red);
        let points = [CanvasPoint::new(0.0, 1.0, "●").style(point_style)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);

        Canvas::new(&points, CanvasBounds::new(0.0, 1.0, 0.0, 1.0))
            .policy(CanvasPolicy {
                explicit_markers: CanvasExplicitMarkers::Rasterize,
                ..CanvasPolicy::auto().prefer_braille()
            })
            .render(Rect::new(0, 0, 1, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("⠁"));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).map(|cell| cell.style),
            Some(point_style)
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
            Some("█")
        );
        assert!(frame.buffer().get(Point::new(2, 1)).is_some());
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
