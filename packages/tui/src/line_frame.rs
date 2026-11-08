//! Border and padding framing for rows of already laid-out styled lines.
//!
//! [`Surface`](crate::composition::Surface) frames a measured child component.
//! Retained row producers (transcripts, logs, measured lists) instead own
//! their rows as [`Line`]s that were laid out at a known content width.
//! [`LineFrame`] frames those rows with the same [`Border`] model — any
//! [`BorderSet`](crate::chrome::BorderSet), partial [`BorderSides`] such as a
//! left-only accent, and per-side styles — and reports one
//! [`LineFrameGeometry`] from which callers derive the producer content width,
//! the content prefix width, and hit-region offsets. Framing never reflows:
//! rows keep their count and residual overflow follows an explicit
//! [`LineFrameOverflow`] policy.
//!
//! [`BorderSides`]: crate::chrome::BorderSides

use std::ops::Range;

use crate::chrome::{Border, BorderSide};
use crate::geometry::Insets;
use crate::style::Style;
use crate::text::{Line, Span, line_viewport, truncate_line_to_display_width};

/// How wide a [`LineFrame`] paints relative to the available width.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LineFrameWidth {
    /// Shrink to the widest row plus insets, capped at the available width.
    #[default]
    Content,
    /// Always occupy the full available width.
    Full,
}

/// How a [`LineFrame`] contains rows wider than the content width.
///
/// Rows are never reflowed; producers should lay out at
/// [`LineFrame::max_content_width`] so this only governs residual overflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LineFrameOverflow {
    /// Clip at the content edge. A wide grapheme that would straddle the edge
    /// is omitted and its cells are filled, keeping the right inset aligned.
    #[default]
    Clip,
    /// Truncate with a trailing ellipsis.
    Ellipsis,
}

/// Border, padding, and fill applied around rows of styled lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFrame {
    /// Border to draw; `None` draws only padding and fill.
    pub border: Option<Border>,
    /// Padding between the border and the content, in terminal cells.
    pub padding: Insets,
    /// Fill style for padding cells and the fallback style behind content.
    /// Border styles are patched over it.
    pub fill: Style,
    /// Width behavior.
    pub width: LineFrameWidth,
    /// Residual overflow behavior.
    pub overflow: LineFrameOverflow,
}

impl Default for LineFrame {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolved horizontal and vertical geometry of one framed block.
///
/// Every cell count here comes from one allocation, so prefix widths, content
/// constraints, painted rows, and hit-region offsets agree by construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LineFrameGeometry {
    /// Total painted width in terminal cells.
    pub width: usize,
    /// Left border cells (0 or 1).
    pub left_border: usize,
    /// Left padding cells.
    pub left_padding: usize,
    /// Content cells.
    pub content_width: usize,
    /// Right padding cells.
    pub right_padding: usize,
    /// Right border cells (0 or 1).
    pub right_border: usize,
    /// Rows above the content (top border plus top padding).
    pub top_rows: usize,
    /// Rows below the content (bottom padding plus bottom border).
    pub bottom_rows: usize,
}

impl LineFrameGeometry {
    /// Column where content starts: the content prefix width.
    #[must_use]
    pub const fn content_column(&self) -> usize {
        self.left_border.saturating_add(self.left_padding)
    }

    /// Cells after the content: right padding plus right border.
    #[must_use]
    pub const fn trailing_width(&self) -> usize {
        self.right_padding.saturating_add(self.right_border)
    }

    /// Output row range holding `content_rows` source rows.
    #[must_use]
    pub const fn content_row_range(&self, content_rows: usize) -> Range<usize> {
        self.top_rows..self.top_rows.saturating_add(content_rows)
    }

    /// Total output rows for `content_rows` source rows.
    #[must_use]
    pub const fn total_rows(&self, content_rows: usize) -> usize {
        self.top_rows
            .saturating_add(content_rows)
            .saturating_add(self.bottom_rows)
    }
}

/// Framed rows together with the geometry that produced them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FramedLines {
    /// Output rows, each exactly [`LineFrameGeometry::width`] cells wide.
    pub lines: Vec<Line>,
    /// Geometry used to paint [`Self::lines`].
    pub geometry: LineFrameGeometry,
}

impl LineFrame {
    /// Create a frame without border or padding.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border: None,
            padding: Insets::new(0, 0, 0, 0),
            fill: Style::new(),
            width: LineFrameWidth::Content,
            overflow: LineFrameOverflow::Clip,
        }
    }

    /// Set the border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// Set the padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Set the fill style.
    #[must_use]
    pub const fn fill(mut self, fill: Style) -> Self {
        self.fill = fill;
        self
    }

    /// Set the width behavior.
    #[must_use]
    pub const fn width(mut self, width: LineFrameWidth) -> Self {
        self.width = width;
        self
    }

    /// Set the residual overflow behavior.
    #[must_use]
    pub const fn overflow(mut self, overflow: LineFrameOverflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// Insets occupied by the border sides plus padding, before narrow-width
    /// degradation.
    #[must_use]
    pub fn insets(&self) -> Insets {
        let border = self
            .border
            .as_ref()
            .map_or_else(Insets::default, |border| border.sides.insets());
        Insets::new(
            border.top.saturating_add(self.padding.top),
            border.right.saturating_add(self.padding.right),
            border.bottom.saturating_add(self.padding.bottom),
            border.left.saturating_add(self.padding.left),
        )
    }

    /// Resolve geometry for rows whose widest row is `natural_content_width`
    /// cells, within `available_width` cells.
    ///
    /// Narrow degradation is explicit: cells go first to the left border, then
    /// the right border, then left padding, right padding, and finally content.
    /// No part ever exceeds `available_width`.
    #[must_use]
    pub fn geometry(
        &self,
        natural_content_width: usize,
        available_width: usize,
    ) -> LineFrameGeometry {
        let border_sides = self.border.as_ref().map(|border| border.sides);
        let wants_left = border_sides.is_some_and(|sides| sides.left);
        let wants_right = border_sides.is_some_and(|sides| sides.right);
        let wants_top = border_sides.is_some_and(|sides| sides.top);
        let wants_bottom = border_sides.is_some_and(|sides| sides.bottom);
        let insets = self.insets();
        let width = match self.width {
            LineFrameWidth::Full => available_width,
            LineFrameWidth::Content => natural_content_width
                .saturating_add(usize::from(insets.horizontal()))
                .min(available_width),
        };
        let mut remaining = width;
        let mut take = |wanted: usize| {
            let granted = wanted.min(remaining);
            remaining -= granted;
            granted
        };
        let left_border = take(usize::from(wants_left));
        let right_border = take(usize::from(wants_right));
        let left_padding = take(usize::from(self.padding.left));
        let right_padding = take(usize::from(self.padding.right));
        let content_width = remaining;
        LineFrameGeometry {
            width,
            left_border,
            left_padding,
            content_width,
            right_padding,
            right_border,
            top_rows: usize::from(wants_top).saturating_add(usize::from(self.padding.top)),
            bottom_rows: usize::from(wants_bottom).saturating_add(usize::from(self.padding.bottom)),
        }
    }

    /// Widest content a producer should lay out at for `available_width`.
    #[must_use]
    pub fn max_content_width(&self, available_width: usize) -> usize {
        self.geometry(available_width, available_width)
            .content_width
    }

    /// Frame `rows` within `available_width` terminal cells.
    ///
    /// The output holds [`LineFrameGeometry::total_rows`] rows, each exactly
    /// [`LineFrameGeometry::width`] cells wide. Source row `i` appears at
    /// output row `geometry.top_rows + i`, starting at
    /// [`LineFrameGeometry::content_column`].
    #[must_use]
    pub fn frame(&self, rows: &[Line], available_width: usize) -> FramedLines {
        let natural = rows.iter().map(Line::width).max().unwrap_or_default();
        let geometry = self.geometry(natural, available_width);
        let height = geometry.total_rows(rows.len());
        let painter = EdgePainter {
            frame: self,
            geometry,
            diagonal: geometry
                .width
                .saturating_sub(1)
                .saturating_add(height.saturating_sub(1)),
        };
        let mut lines = Vec::with_capacity(height);
        let mut y = 0usize;
        let has_top_border = self.border.as_ref().is_some_and(|border| border.sides.top);
        if has_top_border {
            lines.push(painter.horizontal_edge(BorderSide::Top, y));
            y += 1;
        }
        for _ in 0..self.padding.top {
            lines.push(painter.content_row(&Line::new(), y));
            y += 1;
        }
        for row in rows {
            lines.push(painter.content_row(row, y));
            y += 1;
        }
        for _ in 0..self.padding.bottom {
            lines.push(painter.content_row(&Line::new(), y));
            y += 1;
        }
        if self
            .border
            .as_ref()
            .is_some_and(|border| border.sides.bottom)
        {
            lines.push(painter.horizontal_edge(BorderSide::Bottom, y));
        }
        FramedLines { lines, geometry }
    }
}

struct EdgePainter<'a> {
    frame: &'a LineFrame,
    geometry: LineFrameGeometry,
    diagonal: usize,
}

impl EdgePainter<'_> {
    fn border_style(&self, border: &Border, side: BorderSide, x: usize, y: usize) -> Style {
        let style = self.frame.fill.patch(border.side_style(side));
        border.gradient.map_or(style, |gradient| {
            gradient.style_at(
                style,
                u64::try_from(x.saturating_add(y)).unwrap_or(u64::MAX),
                u64::try_from(self.diagonal).unwrap_or(u64::MAX),
            )
        })
    }

    /// Push `count` copies of `glyph` starting at column `x`, splitting cells
    /// only when a gradient varies the style per cell.
    fn push_glyphs(
        &self,
        spans: &mut Vec<Span>,
        border: &Border,
        side: BorderSide,
        glyph: char,
        columns: Range<usize>,
        y: usize,
    ) {
        let x = columns.start;
        let count = columns.len();
        if count == 0 {
            return;
        }
        if border.gradient.is_none() {
            let style = self.border_style(border, side, x, y);
            spans.push(Span::styled(glyph.to_string().repeat(count), style));
            return;
        }
        for offset in 0..count {
            let style = self.border_style(border, side, x.saturating_add(offset), y);
            spans.push(Span::styled(glyph.to_string(), style));
        }
    }

    fn horizontal_edge(&self, side: BorderSide, y: usize) -> Line {
        let Some(border) = self.frame.border.as_ref() else {
            return Line::new();
        };
        let geometry = self.geometry;
        let (left, right) = match side {
            BorderSide::Bottom => (border.set.bottom_left, border.set.bottom_right),
            _ => (border.set.top_left, border.set.top_right),
        };
        let mut spans = Vec::new();
        self.push_glyphs(
            &mut spans,
            border,
            BorderSide::Left,
            left,
            span_columns(0, geometry.left_border),
            y,
        );
        let middle = geometry
            .width
            .saturating_sub(geometry.left_border)
            .saturating_sub(geometry.right_border);
        self.push_glyphs(
            &mut spans,
            border,
            side,
            border.set.horizontal,
            span_columns(geometry.left_border, middle),
            y,
        );
        self.push_glyphs(
            &mut spans,
            border,
            BorderSide::Right,
            right,
            span_columns(
                geometry.width.saturating_sub(geometry.right_border),
                geometry.right_border,
            ),
            y,
        );
        Line::from_spans(spans)
    }

    fn content_row(&self, row: &Line, y: usize) -> Line {
        let geometry = self.geometry;
        let fill = self.frame.fill;
        let mut spans = Vec::new();
        if let Some(border) = self.frame.border.as_ref() {
            self.push_glyphs(
                &mut spans,
                border,
                BorderSide::Left,
                border.set.vertical,
                span_columns(0, geometry.left_border),
                y,
            );
        }
        push_fill(&mut spans, geometry.left_padding, fill);
        let content = match self.frame.overflow {
            LineFrameOverflow::Clip => line_viewport(row, 0, geometry.content_width),
            LineFrameOverflow::Ellipsis => {
                truncate_line_to_display_width(row, geometry.content_width)
            }
        }
        .with_fallback_style(fill);
        let used = content.width();
        spans.extend(
            content
                .spans
                .into_iter()
                .filter(|span| !span.content.is_empty()),
        );
        push_fill(
            &mut spans,
            geometry
                .content_width
                .saturating_sub(used)
                .saturating_add(geometry.right_padding),
            fill,
        );
        if let Some(border) = self.frame.border.as_ref() {
            self.push_glyphs(
                &mut spans,
                border,
                BorderSide::Right,
                border.set.vertical,
                span_columns(
                    geometry.width.saturating_sub(geometry.right_border),
                    geometry.right_border,
                ),
                y,
            );
        }
        Line::from_spans(spans)
    }
}

const fn span_columns(start: usize, count: usize) -> Range<usize> {
    start..start.saturating_add(count)
}

fn push_fill(spans: &mut Vec<Span>, cells: usize, style: Style) {
    if cells > 0 {
        spans.push(Span::styled(" ".repeat(cells), style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::{BorderSet, BorderSideStyles, BorderSides};
    use crate::gradient::ColorGradient;
    use crate::style::Color;
    use bmux_color::Rgb;

    fn left_accent() -> LineFrame {
        LineFrame::new()
            .border(
                Border::new(BorderSet::THICK, Style::new().fg(Color::Green))
                    .sides(BorderSides::LEFT),
            )
            .padding(Insets::new(0, 1, 0, 1))
    }

    fn panel() -> LineFrame {
        LineFrame::new()
            .border(Border::rounded())
            .padding(Insets::new(0, 1, 0, 1))
            .width(LineFrameWidth::Full)
    }

    fn texts(framed: &FramedLines) -> Vec<String> {
        framed.lines.iter().map(Line::plain_text).collect()
    }

    /// Every content row starts at the geometry's content column, and every
    /// row is exactly the geometry's width.
    fn assert_geometry_agrees(framed: &FramedLines, rows: &[Line]) {
        let geometry = framed.geometry;
        assert_eq!(framed.lines.len(), geometry.total_rows(rows.len()));
        for line in &framed.lines {
            assert_eq!(line.width(), geometry.width, "{:?}", line.plain_text());
        }
        for (source, output) in rows
            .iter()
            .zip(&framed.lines[geometry.content_row_range(rows.len())])
        {
            let prefix = output.viewport(0, geometry.content_column());
            assert_eq!(prefix.width(), geometry.content_column());
            let content = output.viewport(geometry.content_column(), geometry.content_width);
            let expected = source.viewport(0, geometry.content_width);
            assert!(
                content.plain_text().starts_with(&expected.plain_text()),
                "{:?} vs {:?}",
                content.plain_text(),
                expected.plain_text()
            );
        }
    }

    #[test]
    fn left_only_border_uses_the_set_vertical_and_reports_prefix_width() {
        let rows = vec![Line::raw("body"), Line::raw("ok")];
        let framed = left_accent().frame(&rows, 20);
        assert_eq!(texts(&framed), ["┃ body ", "┃ ok   "]);
        assert_eq!(framed.geometry.content_column(), 2);
        assert_eq!(framed.geometry.trailing_width(), 1);
        assert_eq!(framed.lines[0].spans[0].style.fg, Some(Color::Green));
        assert_geometry_agrees(&framed, &rows);
    }

    #[test]
    fn full_panel_draws_configured_corners_and_fills_width() {
        let rows = vec![Line::raw("hi")];
        let framed = panel().frame(&rows, 8);
        assert_eq!(texts(&framed), ["╭──────╮", "│ hi   │", "╰──────╯"]);
        assert_eq!(framed.geometry.content_row_range(1), 1..2);
        assert_geometry_agrees(&framed, &rows);
    }

    #[test]
    fn per_side_styles_color_corners_with_their_vertical_side() {
        let left = Style::new().fg(Color::Green);
        let top = Style::new().fg(Color::Blue);
        let frame = LineFrame::new().border(Border::single().side_styles(BorderSideStyles {
            left: Some(left),
            top: Some(top),
            ..BorderSideStyles::NONE
        }));
        let framed = frame.frame(&[Line::raw("ab")], 10);
        let top_row = &framed.lines[0];
        assert_eq!(top_row.spans[0].content, "┌");
        assert_eq!(top_row.spans[0].style.fg, Some(Color::Green));
        assert_eq!(top_row.spans[1].style.fg, Some(Color::Blue));
        assert_eq!(top_row.spans[2].style.fg, None);
        assert_eq!(framed.lines[1].spans[0].style.fg, Some(Color::Green));
        assert_eq!(framed.lines[2].spans[1].style.fg, None);
    }

    #[test]
    fn fill_sits_behind_content_and_under_border_styles() {
        let fill = Style::new().bg(Color::Blue);
        let content = Style::new().fg(Color::Red);
        let framed = left_accent()
            .fill(fill)
            .frame(&[Line::from_spans([Span::styled("x", content)])], 10);
        let row = &framed.lines[0];
        assert_eq!(row.spans[0].style.bg, Some(Color::Blue));
        assert_eq!(row.spans[0].style.fg, Some(Color::Green));
        let body = row.spans.iter().find(|span| span.content == "x").unwrap();
        assert_eq!(body.style.fg, Some(Color::Red));
        assert_eq!(body.style.bg, Some(Color::Blue));
    }

    #[test]
    fn tiny_widths_degrade_in_a_defined_order_without_overflow() {
        let rows = vec![Line::raw("content")];
        for width in 0..6 {
            let framed = panel().frame(&rows, width);
            assert_eq!(framed.lines.len(), 3, "width {width}");
            assert_geometry_agrees(&framed, &rows);
        }
        let geometry = panel().geometry(7, 0);
        assert_eq!(geometry.content_column(), 0);
        assert_eq!(geometry.content_width, 0);
        let one = panel().geometry(7, 1);
        assert_eq!((one.left_border, one.right_border), (1, 0));
        let three = panel().geometry(7, 3);
        assert_eq!(
            (three.left_border, three.right_border, three.left_padding),
            (1, 1, 1)
        );
        assert_eq!(three.content_width, 0);
        assert_eq!(texts(&panel().frame(&rows, 1)), ["╭", "│", "╰"]);
    }

    #[test]
    fn wide_graphemes_are_clipped_without_splitting_and_keep_the_right_edge() {
        let rows = vec![
            Line::raw("界界界"),
            Line::raw("a😀b"),
            Line::raw("e\u{301}\u{301}x"),
        ];
        let frame = panel().padding(Insets::default());
        let framed = frame.frame(&rows, 7);
        assert_eq!(framed.geometry.content_width, 5);
        assert_eq!(
            texts(&framed)[1..4],
            ["│界界 │", "│a😀b │", "│e\u{301}\u{301}x   │"]
        );
        assert_geometry_agrees(&framed, &rows);
        for width in 0..8 {
            assert_geometry_agrees(&frame.frame(&rows, width), &rows);
        }
    }

    #[test]
    fn ellipsis_overflow_truncates_inside_the_content_width() {
        let rows = vec![Line::raw("abcdefgh")];
        let framed = panel()
            .overflow(LineFrameOverflow::Ellipsis)
            .frame(&rows, 8);
        assert_eq!(texts(&framed)[1], "│ abc… │");
        for line in &framed.lines {
            assert_eq!(line.width(), 8);
        }
    }

    #[test]
    fn content_width_shrinks_to_widest_row_and_producers_get_the_cap() {
        let frame = left_accent();
        let framed = frame.frame(&[Line::raw("界"), Line::raw("abc")], 40);
        assert_eq!(framed.geometry.width, 6);
        assert_eq!(frame.max_content_width(40), 37);
        assert_eq!(frame.max_content_width(2), 0);
    }

    #[test]
    fn padding_rows_and_partial_horizontal_sides_preserve_row_mapping() {
        let frame = LineFrame::new()
            .border(Border::single().sides(BorderSides::new(true, false, false, true)))
            .padding(Insets::new(1, 0, 1, 0));
        let rows = vec![Line::raw("a"), Line::raw("b")];
        let framed = frame.frame(&rows, 10);
        assert_eq!(texts(&framed), ["┌─", "│ ", "│a", "│b", "│ "]);
        assert_eq!(framed.geometry.content_row_range(2), 2..4);
        assert_geometry_agrees(&framed, &rows);
    }

    #[test]
    fn gradient_runs_corner_to_corner_across_framed_rows() {
        let from = Rgb::new(0xff, 0x00, 0xaa);
        let to = Rgb::new(0x00, 0xff, 0xff);
        let frame = panel().border(Border::rounded().gradient(ColorGradient::new(from, to)));
        let framed = frame.frame(&[Line::raw("hi")], 6);
        let first = &framed.lines[0].spans[0];
        let last = framed.lines[2].spans.last().unwrap();
        assert_eq!(first.style.fg, Some(Color::from(from)));
        assert_eq!(last.style.fg, Some(Color::from(to)));
        assert_eq!(framed.lines[0].width(), 6);
    }

    #[test]
    fn empty_rows_and_no_border_produce_only_padding() {
        let frame = LineFrame::new().padding(Insets::new(0, 1, 0, 1));
        assert_eq!(frame.frame(&[], 10).lines.len(), 0);
        let framed = frame.frame(&[Line::raw("x")], 10);
        assert_eq!(texts(&framed), [" x "]);
        assert_eq!(framed.geometry.content_column(), 1);
    }
}
