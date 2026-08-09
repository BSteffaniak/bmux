//! Lightweight generic chart component.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span, display_width};
use bmux_tui::style::{Color, Style};
use bmux_tui::text::truncate_line_to_display_width;
use bmux_tui::text_width::truncate_to_display_width;

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

/// Chart axis label configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartAxis<'a> {
    /// Optional axis title.
    pub title: Option<&'a str>,
    /// Caller-owned axis labels.
    pub labels: &'a [&'a str],
    /// Axis style.
    pub style: Style,
}

impl<'a> ChartAxis<'a> {
    /// Create an axis with no title or labels.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            title: None,
            labels: &[],
            style: Style::new(),
        }
    }

    /// Return this axis with a title.
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Return this axis with labels.
    #[must_use]
    pub const fn labels(mut self, labels: &'a [&'a str]) -> Self {
        self.labels = labels;
        self
    }

    /// Return this axis with style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

/// Chart axes model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartAxes<'a> {
    /// X axis configuration.
    pub x: ChartAxis<'a>,
    /// Y axis configuration.
    pub y: ChartAxis<'a>,
    /// Whether to reserve/render legend metadata.
    pub legend: bool,
}

impl<'a> ChartAxes<'a> {
    /// Create empty axes.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            x: ChartAxis::empty(),
            y: ChartAxis::empty(),
            legend: false,
        }
    }

    /// Return this model with x axis changed.
    #[must_use]
    pub const fn x(mut self, x: ChartAxis<'a>) -> Self {
        self.x = x;
        self
    }

    /// Return this model with y axis changed.
    #[must_use]
    pub const fn y(mut self, y: ChartAxis<'a>) -> Self {
        self.y = y;
        self
    }

    /// Return this model with legend visibility changed.
    #[must_use]
    pub const fn legend(mut self, legend: bool) -> Self {
        self.legend = legend;
        self
    }
}

/// Chart line interpolation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartInterpolation {
    /// Render only dataset markers.
    PointsOnly,
    /// Connect line datasets with straight segments.
    Straight,
    /// Connect line datasets with horizontal-then-vertical step segments.
    Step,
}

/// Chart clipping policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartClipping {
    /// Drop points outside bounds.
    Clip,
    /// Clamp points outside bounds to the nearest chart edge.
    Clamp,
}

/// Chart legend placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartLegendPlacement {
    /// Do not render a legend.
    Hidden,
    /// Render legend metadata at the top-right edge.
    TopRight,
    /// Render legend metadata at the bottom-right edge.
    BottomRight,
}

/// Chart axis visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartAxisVisibility {
    /// Do not render axis metadata.
    Hidden,
    /// Render axis metadata.
    Visible,
}

/// Chart rendering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartPolicy {
    /// Line interpolation mode.
    pub interpolation: ChartInterpolation,
    /// Point clipping behavior.
    pub clipping: ChartClipping,
    /// Legend placement.
    pub legend: ChartLegendPlacement,
    /// Axis visibility.
    pub axes: ChartAxisVisibility,
}

impl ChartPolicy {
    /// Compact default chart policy.
    #[must_use]
    pub const fn compact() -> Self {
        Self {
            interpolation: ChartInterpolation::Straight,
            clipping: ChartClipping::Clip,
            legend: ChartLegendPlacement::Hidden,
            axes: ChartAxisVisibility::Hidden,
        }
    }

    /// Return this policy with interpolation changed.
    #[must_use]
    pub const fn interpolation(mut self, interpolation: ChartInterpolation) -> Self {
        self.interpolation = interpolation;
        self
    }

    /// Return this policy with clipping changed.
    #[must_use]
    pub const fn clipping(mut self, clipping: ChartClipping) -> Self {
        self.clipping = clipping;
        self
    }

    /// Return this policy with legend placement changed.
    #[must_use]
    pub const fn legend(mut self, legend: ChartLegendPlacement) -> Self {
        self.legend = legend;
        self
    }

    /// Return this policy with axis visibility changed.
    #[must_use]
    pub const fn axes(mut self, axes: ChartAxisVisibility) -> Self {
        self.axes = axes;
        self
    }
}

impl Default for ChartPolicy {
    fn default() -> Self {
        Self::compact()
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
    axes: ChartAxes<'a>,
    policy: ChartPolicy,
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
            axes: ChartAxes::empty(),
            policy: ChartPolicy::compact(),
            styles: ChartStyles {
                dataset: Style::new(),
                empty: Style::new(),
            },
            empty: "No data",
        }
    }

    /// Set axes model.
    #[must_use]
    pub const fn axes(mut self, axes: ChartAxes<'a>) -> Self {
        self.axes = axes;
        self
    }

    /// Set chart rendering policy.
    #[must_use]
    pub const fn policy(mut self, policy: ChartPolicy) -> Self {
        self.policy = policy;
        self
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

    /// Return chart policy.
    #[must_use]
    pub const fn policy_model(&self) -> ChartPolicy {
        self.policy
    }

    /// Return chart axes.
    #[must_use]
    pub const fn axes_model(&self) -> ChartAxes<'a> {
        self.axes
    }

    /// Return the plot area after reserving space for visible axes.
    #[must_use]
    pub fn plot_area(&self, area: Rect) -> Rect {
        plot_area(area, self.axes, self.policy.axes)
    }

    /// Map a chart point into an area cell.
    #[must_use]
    pub fn map_point(&self, area: Rect, point: ChartPoint) -> Option<(u16, u16)> {
        map_point(area, self.bounds, point, self.policy.clipping)
    }

    /// Map a chart point into an area cell with an explicit clipping mode.
    #[must_use]
    pub fn map_point_with_clipping(
        &self,
        area: Rect,
        point: ChartPoint,
        clipping: ChartClipping,
    ) -> Option<(u16, u16)> {
        map_point(area, self.bounds, point, clipping)
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
        if matches!(self.policy.axes, ChartAxisVisibility::Visible) {
            render_axes(frame, area, self.axes, self.styles.dataset);
        }
        let plot_area = self.plot_area(area);
        for dataset in self.datasets {
            let style = if dataset.style == Style::new() {
                self.styles.dataset
            } else {
                dataset.style
            };
            if matches!(dataset.kind, ChartDatasetKind::Line) {
                match self.policy.interpolation {
                    ChartInterpolation::PointsOnly => {}
                    ChartInterpolation::Straight => {
                        for pair in dataset.points.windows(2) {
                            if let (Some((x0, y0)), Some((x1, y1))) = (
                                self.map_point(plot_area, pair[0]),
                                self.map_point(plot_area, pair[1]),
                            ) {
                                draw_line(
                                    frame,
                                    plot_area,
                                    (x0, y0),
                                    (x1, y1),
                                    dataset.marker,
                                    style,
                                );
                            }
                        }
                    }
                    ChartInterpolation::Step => {
                        for pair in dataset.points.windows(2) {
                            if let (Some((x0, y0)), Some((x1, y1))) = (
                                self.map_point(plot_area, pair[0]),
                                self.map_point(plot_area, pair[1]),
                            ) {
                                let corner = (x1, y0);
                                draw_line(
                                    frame,
                                    plot_area,
                                    (x0, y0),
                                    corner,
                                    dataset.marker,
                                    style,
                                );
                                draw_line(
                                    frame,
                                    plot_area,
                                    corner,
                                    (x1, y1),
                                    dataset.marker,
                                    style,
                                );
                            }
                        }
                    }
                }
            }
            for point in dataset.points {
                if let Some((x, y)) = self.map_point(plot_area, *point) {
                    draw_cell(frame, plot_area, x, y, dataset.marker, style);
                }
            }
        }
        if !matches!(self.policy.legend, ChartLegendPlacement::Hidden) {
            render_legend(
                frame,
                area,
                self.datasets,
                self.policy.legend,
                self.axes,
                self.policy.axes,
                self.styles.dataset,
            );
        }
    }
}

fn plot_area(area: Rect, axes: ChartAxes<'_>, visibility: ChartAxisVisibility) -> Rect {
    if area.is_empty() || matches!(visibility, ChartAxisVisibility::Hidden) {
        return area;
    }
    let top = u16::from(axes.y.title.is_some());
    let bottom = u16::from(axes.x.title.is_some() || !axes.x.labels.is_empty());
    let left = u16::from(!axes.y.labels.is_empty());
    Rect::new(
        area.x.saturating_add(left),
        area.y.saturating_add(top),
        area.width.saturating_sub(left),
        area.height.saturating_sub(top.saturating_add(bottom)),
    )
}

fn render_axes(frame: &mut Frame<'_>, area: Rect, axes: ChartAxes<'_>, style: Style) {
    if let Some(title) = axes.x.title {
        frame.write_line(
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            &Line::from_spans([Span::styled(title, axes.x.style_or(style))]),
        );
    }
    if let Some(title) = axes.y.title {
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans([Span::styled(title, axes.y.style_or(style))]),
        );
    }
    if !axes.x.labels.is_empty() {
        render_even_labels(
            frame,
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            axes.x.labels,
            axes.x.style_or(style),
        );
    }
    if !axes.y.labels.is_empty() {
        render_vertical_labels(frame, area, axes.y.labels, axes.y.style_or(style));
    }
}

fn render_even_labels(frame: &mut Frame<'_>, area: Rect, labels: &[&str], style: Style) {
    if area.is_empty() || labels.is_empty() {
        return;
    }
    let last = labels.len().saturating_sub(1);
    let mut next_free_x = area.x;
    for (index, label) in labels.iter().enumerate() {
        let x_offset = if last == 0 {
            0
        } else {
            u16::try_from(
                (index * usize::from(area.width.saturating_sub(1)))
                    .checked_div(last)
                    .unwrap_or(0),
            )
            .unwrap_or(u16::MAX)
        };
        let x = area.x.saturating_add(x_offset);
        if x < next_free_x {
            continue;
        }
        let available = usize::from(area.right().saturating_sub(x));
        let label = truncate_to_display_width(label, available);
        let width = u16::try_from(display_width(&label)).unwrap_or(u16::MAX);
        if width == 0 {
            continue;
        }
        frame.write_line(
            Rect::new(x, area.y, area.right().saturating_sub(x), 1),
            &Line::from_spans([Span::styled(label, style)]),
        );
        next_free_x = x.saturating_add(width).saturating_add(1);
    }
}

fn render_vertical_labels(frame: &mut Frame<'_>, area: Rect, labels: &[&str], style: Style) {
    if area.is_empty() || labels.is_empty() {
        return;
    }
    let last = labels.len().saturating_sub(1);
    let mut last_y = None;
    for (index, label) in labels.iter().enumerate() {
        let y_offset = if last == 0 {
            0
        } else {
            u16::try_from(
                (index * usize::from(area.height.saturating_sub(1)))
                    .checked_div(last)
                    .unwrap_or(0),
            )
            .unwrap_or(u16::MAX)
        };
        let y = area.y.saturating_add(y_offset);
        if last_y == Some(y) {
            continue;
        }
        last_y = Some(y);
        let label = truncate_to_display_width(label, usize::from(area.width));
        frame.write_line(
            Rect::new(area.x, y, area.width, 1),
            &Line::from_spans([Span::styled(label, style)]),
        );
    }
}

fn render_legend(
    frame: &mut Frame<'_>,
    area: Rect,
    datasets: &[ChartDataset<'_>],
    placement: ChartLegendPlacement,
    axes: ChartAxes<'_>,
    axis_visibility: ChartAxisVisibility,
    style: Style,
) {
    let mut spans = Vec::new();
    for dataset in datasets.iter().filter(|dataset| !dataset.name.is_empty()) {
        if !spans.is_empty() {
            spans.push(Span::styled(" ", style));
        }
        let dataset_style = if dataset.style == Style::new() {
            style
        } else {
            dataset.style
        };
        spans.push(Span::styled(dataset.name, dataset_style));
    }
    if spans.is_empty() {
        return;
    }
    let line = Line::from_spans(spans);
    let y = match placement {
        ChartLegendPlacement::Hidden => return,
        ChartLegendPlacement::TopRight => area.y.saturating_add(u16::from(
            matches!(axis_visibility, ChartAxisVisibility::Visible) && axes.y.title.is_some(),
        )),
        ChartLegendPlacement::BottomRight => {
            area.bottom().saturating_sub(1).saturating_sub(u16::from(
                matches!(axis_visibility, ChartAxisVisibility::Visible)
                    && (axes.x.title.is_some() || !axes.x.labels.is_empty()),
            ))
        }
    };
    let width = u16::try_from(display_width(&line.plain_text())).unwrap_or(u16::MAX);
    let x = area.right().saturating_sub(width).max(area.x);
    let available = usize::from(area.right().saturating_sub(x));
    let line = truncate_line_to_display_width(&line, available);
    frame.write_line(Rect::new(x, y, area.right().saturating_sub(x), 1), &line);
}

impl ChartAxis<'_> {
    fn style_or(self, fallback: Style) -> Style {
        if self.style == Style::new() {
            fallback
        } else {
            self.style
        }
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
            draw_cell(frame, area, x, y, marker, style);
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

fn draw_cell(frame: &mut Frame<'_>, area: Rect, x: u16, y: u16, marker: &str, style: Style) {
    frame.buffer_mut().set_cell(
        Point::new(area.x.saturating_add(x), area.y.saturating_add(y)),
        marker,
        style,
    );
}

fn rounded_u16(value: f64) -> u16 {
    value
        .round()
        .clamp(0.0, f64::from(u16::MAX))
        .to_string()
        .parse::<u16>()
        .unwrap_or(0)
}

fn map_point(
    area: Rect,
    bounds: ChartBounds,
    mut point: ChartPoint,
    clipping: ChartClipping,
) -> Option<(u16, u16)> {
    if area.is_empty() || bounds.x_min >= bounds.x_max || bounds.y_min >= bounds.y_max {
        return None;
    }
    match clipping {
        ChartClipping::Clip
            if point.x < bounds.x_min
                || point.x > bounds.x_max
                || point.y < bounds.y_min
                || point.y > bounds.y_max =>
        {
            return None;
        }
        ChartClipping::Clip => {}
        ChartClipping::Clamp => {
            point.x = point.x.clamp(bounds.x_min, bounds.x_max);
            point.y = point.y.clamp(bounds.y_min, bounds.y_max);
        }
    }
    let x_span = bounds.x_max - bounds.x_min;
    let y_span = bounds.y_max - bounds.y_min;
    let x = (point.x - bounds.x_min) / x_span * f64::from(area.width.saturating_sub(1));
    let y = (bounds.y_max - point.y) / y_span * f64::from(area.height.saturating_sub(1));
    Some((rounded_u16(x), rounded_u16(y)))
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ChartStyles`].
    #[must_use]
    pub fn chart_styles(self) -> ChartStyles {
        ChartStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ChartStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        Self {
            dataset: theme.info,
            empty: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point as TuiPoint, Rect};
    use bmux_tui::style::{Color, Style};

    use super::{
        Chart, ChartAxes, ChartAxis, ChartAxisVisibility, ChartBounds, ChartClipping, ChartDataset,
        ChartInterpolation, ChartLegendPlacement, ChartPoint, ChartPolicy, render_even_labels,
        render_vertical_labels,
    };

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
    fn clips_or_clamps_points_to_bounds() {
        let points = [ChartPoint::new(12.0, -1.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let chart = Chart::new(&datasets, ChartBounds::new(0.0, 10.0, 0.0, 10.0));

        assert_eq!(
            chart.map_point_with_clipping(Rect::new(0, 0, 11, 11), points[0], ChartClipping::Clip),
            None
        );
        assert_eq!(
            chart.map_point_with_clipping(Rect::new(0, 0, 11, 11), points[0], ChartClipping::Clamp),
            Some((10, 10))
        );
    }

    #[test]
    fn reserves_plot_area_for_visible_axes() {
        let points = [ChartPoint::new(1.0, 1.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let y_labels = ["hi", "lo"];
        let chart = Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .axes(
                ChartAxes::empty()
                    .x(ChartAxis::empty().title("x").labels(&["0", "2"]))
                    .y(ChartAxis::empty().title("y").labels(&y_labels)),
            )
            .policy(ChartPolicy::compact().axes(ChartAxisVisibility::Visible));

        assert_eq!(
            chart.plot_area(Rect::new(0, 0, 10, 5)),
            Rect::new(1, 1, 9, 3)
        );
    }

    #[test]
    fn renders_axis_tick_labels() {
        let points = [ChartPoint::new(1.0, 1.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let x_labels = ["0", "2"];
        let y_labels = ["hi", "lo"];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .axes(
                ChartAxes::empty()
                    .x(ChartAxis::empty().labels(&x_labels))
                    .y(ChartAxis::empty().labels(&y_labels)),
            )
            .policy(ChartPolicy::compact().axes(ChartAxisVisibility::Visible))
            .render(Rect::new(0, 0, 8, 4), &mut frame);

        assert!(
            frame
                .buffer()
                .row_symbols(0)
                .as_deref()
                .is_some_and(|row| row.contains("hi"))
        );
        assert!(
            frame
                .buffer()
                .row_symbols(3)
                .as_deref()
                .is_some_and(|row| row.contains('2'))
        );
    }

    #[test]
    fn places_legend_without_axis_row_collision() {
        let points = [ChartPoint::new(1.0, 1.0)];
        let datasets = [ChartDataset::scatter("series", &points)];
        let axes = ChartAxes::empty()
            .x(ChartAxis::empty().title("x axis"))
            .y(ChartAxis::empty().title("y axis"));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 4));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .axes(axes)
            .policy(
                ChartPolicy::compact()
                    .axes(ChartAxisVisibility::Visible)
                    .legend(ChartLegendPlacement::TopRight),
            )
            .render(Rect::new(0, 0, 14, 4), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("y axis        ")
        );
        assert!(
            frame
                .buffer()
                .row_symbols(1)
                .as_deref()
                .is_some_and(|row| row.ends_with("series"))
        );
    }

    #[test]
    fn hides_legend_in_tiny_empty_areas() {
        let points = [ChartPoint::new(0.0, 0.0)];
        let datasets = [ChartDataset::scatter("series", &points)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .policy(ChartPolicy::compact().legend(ChartLegendPlacement::TopRight))
            .render(Rect::new(0, 0, 0, 0), &mut frame);
    }

    #[test]
    fn stores_axes_labels_titles_and_legend_policy() {
        let points = [ChartPoint::new(0.0, 0.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let x_labels = ["0", "10"];
        let y_labels = ["low", "high"];
        let axes = ChartAxes::empty()
            .x(ChartAxis::empty().title("time").labels(&x_labels))
            .y(ChartAxis::empty().title("value").labels(&y_labels))
            .legend(true);

        let chart = Chart::new(&datasets, ChartBounds::new(0.0, 10.0, 0.0, 10.0)).axes(axes);

        assert_eq!(chart.axes_model().x.title, Some("time"));
        assert_eq!(chart.axes_model().y.labels, y_labels);
        assert!(chart.axes_model().legend);
    }

    #[test]
    fn stores_chart_rendering_policy() {
        let points = [ChartPoint::new(0.0, 0.0)];
        let datasets = [ChartDataset::line("line", &points)];
        let policy = ChartPolicy::compact()
            .interpolation(ChartInterpolation::PointsOnly)
            .clipping(ChartClipping::Clamp)
            .legend(ChartLegendPlacement::TopRight)
            .axes(ChartAxisVisibility::Visible);

        let chart = Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0)).policy(policy);

        assert_eq!(chart.policy_model(), policy);
    }

    #[test]
    fn renders_line_dataset_segments() {
        let points = [ChartPoint::new(0.0, 0.0), ChartPoint::new(2.0, 2.0)];
        let datasets = [ChartDataset::line("line", &points).marker("*")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 3));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .render(Rect::new(0, 0, 3, 3), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(0, 2))
                .map(|cell| cell.symbol.as_str()),
            Some("*")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(1, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("*")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(2, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("*")
        );
    }

    #[test]
    fn renders_axes_titles_and_legend() {
        let points = [ChartPoint::new(1.0, 1.0)];
        let datasets = [ChartDataset::scatter("series", &points).marker("x")];
        let axes = ChartAxes::empty()
            .x(ChartAxis::empty().title("x axis"))
            .y(ChartAxis::empty().title("y axis"));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 4));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .axes(axes)
            .policy(
                ChartPolicy::compact()
                    .axes(ChartAxisVisibility::Visible)
                    .legend(ChartLegendPlacement::BottomRight),
            )
            .render(Rect::new(0, 0, 12, 4), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("y axis      ")
        );
        assert!(
            frame
                .buffer()
                .row_symbols(2)
                .as_deref()
                .is_some_and(|row| row.ends_with("series"))
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("x axis      ")
        );
    }

    #[test]
    fn renders_step_interpolation_segments() {
        let points = [ChartPoint::new(0.0, 0.0), ChartPoint::new(2.0, 2.0)];
        let datasets = [ChartDataset::line("step", &points).marker("s")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 3));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 2.0, 0.0, 2.0))
            .policy(ChartPolicy::compact().interpolation(ChartInterpolation::Step))
            .render(Rect::new(0, 0, 3, 3), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(0, 2))
                .map(|cell| cell.symbol.as_str()),
            Some("s")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(2, 2))
                .map(|cell| cell.symbol.as_str()),
            Some("s")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(2, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("s")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(1, 1))
                .map(|cell| cell.symbol.as_str()),
            Some(" ")
        );
    }

    #[test]
    fn renders_empty_data_message() {
        let datasets = [ChartDataset::scatter("empty", &[])];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .render(Rect::new(0, 0, 8, 1), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("No data "));
    }

    #[test]
    fn legend_truncates_to_area_and_preserves_dataset_styles() {
        let points_a = [ChartPoint::new(0.0, 0.0)];
        let points_b = [ChartPoint::new(1.0, 1.0)];
        let datasets = [
            ChartDataset::scatter("alpha", &points_a).style(Style::new().fg(Color::Red)),
            ChartDataset::scatter("beta", &points_b),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .styles(super::ChartStyles {
                dataset: Style::new().fg(Color::Blue),
                empty: Style::new(),
            })
            .policy(ChartPolicy::compact().legend(ChartLegendPlacement::TopRight))
            .render(Rect::new(0, 0, 8, 2), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("alpha b…"));
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(0, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Red))
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(6, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Blue))
        );
    }

    #[test]
    fn dataset_style_overrides_chart_fallback_style() {
        let points_a = [ChartPoint::new(0.0, 0.0)];
        let points_b = [ChartPoint::new(1.0, 1.0)];
        let datasets = [
            ChartDataset::scatter("fallback", &points_a),
            ChartDataset::scatter("override", &points_b).style(Style::new().fg(Color::Red)),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .styles(super::ChartStyles {
                dataset: Style::new().fg(Color::Blue),
                empty: Style::new(),
            })
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(0, 1))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Blue))
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(1, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Red))
        );
    }

    #[test]
    fn multiple_datasets_and_unicode_markers_render() {
        let points_a = [ChartPoint::new(0.0, 0.0)];
        let points_b = [ChartPoint::new(1.0, 1.0)];
        let datasets = [
            ChartDataset::scatter("a", &points_a).marker("◆"),
            ChartDataset::scatter("b", &points_b).marker("○"),
        ];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .render(Rect::new(0, 0, 2, 2), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(0, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("◆")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(1, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("○")
        );
    }

    #[test]
    fn axis_labels_do_not_collide_in_small_widths() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        render_even_labels(
            &mut frame,
            Rect::new(0, 0, 8, 1),
            &["first", "second", "third"],
            Style::new(),
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("first  …"));
    }

    #[test]
    fn vertical_axis_labels_skip_duplicate_rows_and_truncate() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        let mut frame = Frame::new(&mut buffer);

        render_vertical_labels(
            &mut frame,
            Rect::new(0, 0, 4, 2),
            &["long", "skip", "tail"],
            Style::new(),
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("long"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("tail"));
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let points = [ChartPoint::new(0.0, 0.0)];
        let datasets = [ChartDataset::scatter("points", &points)];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Chart::new(&datasets, ChartBounds::new(0.0, 1.0, 0.0, 1.0))
            .render(Rect::new(0, 0, 0, 0), &mut frame);
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
                .get(TuiPoint::new(0, 10))
                .map(|cell| cell.symbol.as_str()),
            Some("x")
        );
        assert_eq!(
            frame
                .buffer()
                .get(TuiPoint::new(10, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("x")
        );
    }
}
