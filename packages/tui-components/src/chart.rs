//! Lightweight generic chart component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Style};

/// One chart point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartPoint {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

impl ChartPoint {
    /// Create a chart point.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Chart dataset rendering kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartDatasetKind {
    /// Render independent point markers.
    Scatter,
    /// Render point markers connected by simple line segments.
    Line,
}

/// One named chart dataset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartDataset<'a> {
    /// Dataset name.
    pub name: &'a str,
    /// Caller-owned points.
    pub points: &'a [ChartPoint],
    /// Dataset kind.
    pub kind: ChartDatasetKind,
    /// Marker symbol.
    pub marker: &'a str,
    /// Dataset style.
    pub style: Style,
}

impl<'a> ChartDataset<'a> {
    /// Create a scatter dataset.
    #[must_use]
    pub const fn scatter(name: &'a str, points: &'a [ChartPoint]) -> Self {
        Self {
            name,
            points,
            kind: ChartDatasetKind::Scatter,
            marker: "•",
            style: Style::new(),
        }
    }

    /// Create a line dataset.
    #[must_use]
    pub const fn line(name: &'a str, points: &'a [ChartPoint]) -> Self {
        Self {
            name,
            points,
            kind: ChartDatasetKind::Line,
            marker: "•",
            style: Style::new(),
        }
    }

    /// Return this dataset with a custom marker.
    #[must_use]
    pub const fn marker(mut self, marker: &'a str) -> Self {
        self.marker = marker;
        self
    }

    /// Return this dataset with a custom style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Chart axis bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartBounds {
    /// Minimum x value.
    pub x_min: f64,
    /// Maximum x value.
    pub x_max: f64,
    /// Minimum y value.
    pub y_min: f64,
    /// Maximum y value.
    pub y_max: f64,
}

impl ChartBounds {
    /// Create chart bounds.
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

/// Chart visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartStyles {
    /// Fallback dataset style.
    pub dataset: Style,
    /// Empty chart style.
    pub empty: Style,
}

impl Default for ChartStyles {
    fn default() -> Self {
        Self {
            dataset: Style::new().fg(Color::Cyan),
            empty: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Lightweight scatter/line chart over caller-owned datasets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chart<'a> {
    datasets: &'a [ChartDataset<'a>],
    bounds: ChartBounds,
    styles: ChartStyles,
    empty: &'a str,
}

impl<'a> Chart<'a> {
    /// Create a chart with explicit bounds.
    #[must_use]
    pub const fn new(datasets: &'a [ChartDataset<'a>], bounds: ChartBounds) -> Self {
        Self {
            datasets,
            bounds,
            styles: ChartStyles {
                dataset: Style::new(),
                empty: Style::new(),
            },
            empty: "No data",
        }
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ChartStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }

    /// Map a chart point into an area cell.
    #[must_use]
    pub fn map_point(&self, area: Rect, point: ChartPoint) -> Option<(u16, u16)> {
        map_point(area, self.bounds, point)
    }

    /// Render chart datasets.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        if self
            .datasets
            .iter()
            .all(|dataset| dataset.points.is_empty())
        {
            frame.write_line_with_fallback_style(
                area,
                &bmux_tui::prelude::Line::from(self.empty),
                self.styles.empty,
            );
            return;
        }
        for dataset in self.datasets {
            let style = if dataset.style == Style::new() {
                self.styles.dataset
            } else {
                dataset.style
            };
            for point in dataset.points {
                if let Some((x, y)) = self.map_point(area, *point) {
                    frame.buffer_mut().set_cell(
                        bmux_tui::geometry::Point::new(
                            area.x.saturating_add(x),
                            area.y.saturating_add(y),
                        ),
                        dataset.marker,
                        style,
                    );
                }
            }
        }
    }
}

fn rounded_u16(value: f64) -> u16 {
    value
        .round()
        .clamp(0.0, f64::from(u16::MAX))
        .to_string()
        .parse::<u16>()
        .unwrap_or(0)
}

fn map_point(area: Rect, bounds: ChartBounds, point: ChartPoint) -> Option<(u16, u16)> {
    if area.is_empty()
        || bounds.x_min >= bounds.x_max
        || bounds.y_min >= bounds.y_max
        || point.x < bounds.x_min
        || point.x > bounds.x_max
        || point.y < bounds.y_min
        || point.y > bounds.y_max
    {
        return None;
    }
    let x_span = bounds.x_max - bounds.x_min;
    let y_span = bounds.y_max - bounds.y_min;
    let x = (point.x - bounds.x_min) / x_span * f64::from(area.width.saturating_sub(1));
    let y = (bounds.y_max - point.y) / y_span * f64::from(area.height.saturating_sub(1));
    Some((rounded_u16(x), rounded_u16(y)))
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{Chart, ChartBounds, ChartDataset, ChartPoint};

    #[test]
    fn maps_points_to_chart_cells() {
        let points = [ChartPoint::new(5.0, 5.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let chart = Chart::new(&datasets, ChartBounds::new(0.0, 10.0, 0.0, 10.0));

        assert_eq!(
            chart.map_point(Rect::new(0, 0, 11, 11), points[0]),
            Some((5, 5))
        );
    }

    #[test]
    fn renders_scatter_dataset() {
        let points = [ChartPoint::new(0.0, 0.0), ChartPoint::new(10.0, 10.0)];
        let datasets = [ChartDataset::scatter("points", &points).marker("x")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 11));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 10.0, 0.0, 10.0))
            .render(Rect::new(0, 0, 11, 11), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 10))
                .map(|cell| cell.symbol.as_str()),
            Some("x")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(10, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("x")
        );
    }
}
