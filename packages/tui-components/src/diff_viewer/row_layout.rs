//! Row-level layout of one logical diff item.
//!
//! A virtualized caller lays out only the items it shows. Every row produced
//! here is exactly the requested width, and the source mapping is produced by
//! the same pass that places graphemes into cells, so painting, hit testing,
//! and selection geometry agree.

use std::ops::Range;

use bmux_tui::prelude::{Line, Modifier, Span, Style};
use bmux_tui::text_width::painted_grapheme_width;
use unicode_segmentation::UnicodeSegmentation;

use super::{ChangedRange, DiffLineKind, DiffSourceSide, DiffViewerStyle};

/// Caller-supplied style for a byte range of one content line, such as a
/// syntax token.
///
/// Ranges are UTF-8 byte offsets into the line text. They must be sorted by
/// `start` and must not overlap; a grapheme takes the style of the range that
/// contains its first byte. Ranges outside the text have no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSyntaxRange {
    /// First byte of the range.
    pub start: usize,
    /// Byte after the last byte of the range.
    pub end: usize,
    /// Style patched over the line's base content style.
    pub style: Style,
}

impl DiffSyntaxRange {
    /// Create a styled byte range.
    #[must_use]
    pub const fn new(start: usize, end: usize, style: Style) -> Self {
        Self { start, end, style }
    }
}

/// Content of one side of a diff item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRowText<'a> {
    /// Line text without its line terminator.
    pub text: &'a str,
    /// Byte ranges emphasized as changed within the line, sorted by `start`
    /// and disjoint. A grapheme overlapping a range is emphasized.
    pub changed_ranges: &'a [ChangedRange],
    /// Caller-supplied styled ranges, sorted by `start` and disjoint.
    pub syntax: &'a [DiffSyntaxRange],
}

impl<'a> DiffRowText<'a> {
    /// Unstyled text without intra-line emphasis.
    #[must_use]
    pub const fn plain(text: &'a str) -> Self {
        Self {
            text,
            changed_ranges: &[],
            syntax: &[],
        }
    }

    /// Return this text with intra-line changed ranges.
    #[must_use]
    pub const fn with_changed_ranges(mut self, changed_ranges: &'a [ChangedRange]) -> Self {
        self.changed_ranges = changed_ranges;
        self
    }

    /// Return this text with caller-supplied styled ranges.
    #[must_use]
    pub const fn with_syntax(mut self, syntax: &'a [DiffSyntaxRange]) -> Self {
        self.syntax = syntax;
        self
    }
}

/// One side of a side-by-side pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRowCell<'a> {
    /// Kind used for the marker and row styles.
    pub kind: DiffLineKind,
    /// Line number shown in this side's gutter.
    pub line: Option<u32>,
    /// Content of this side.
    pub text: DiffRowText<'a>,
}

impl<'a> DiffRowCell<'a> {
    /// Create a side-by-side cell.
    #[must_use]
    pub const fn new(kind: DiffLineKind, line: Option<u32>, text: DiffRowText<'a>) -> Self {
        Self { kind, line, text }
    }
}

/// One logical diff item laid out by [`layout_diff_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowItem<'a> {
    /// One line of a unified diff.
    ///
    /// Removed lines map to the old side. Other lines map to the new side,
    /// or to the old side when only an old line number is known.
    Unified {
        /// Kind used for the marker and row styles.
        kind: DiffLineKind,
        /// Old-side line number.
        old_line: Option<u32>,
        /// New-side line number.
        new_line: Option<u32>,
        /// Line content.
        text: DiffRowText<'a>,
    },
    /// One aligned pair of a side-by-side diff. Either side may be absent;
    /// an absent side renders as blank cells without a source mapping.
    SideBySide {
        /// Old-side cell, mapped to [`DiffSourceSide::Old`].
        old: Option<DiffRowCell<'a>>,
        /// New-side cell, mapped to [`DiffSourceSide::New`].
        new: Option<DiffRowCell<'a>>,
    },
}

/// Arrangement of line numbers and the change marker in a gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffGutterShape {
    /// Line numbers, then the marker.
    ///
    /// Unified rows show ` OLD NEW + ` (both numbers, blank when unknown);
    /// side-by-side cells show ` NUM + `.
    #[default]
    LineNumbersThenMarker,
    /// The marker, then one line number, then a separator.
    ///
    /// Unified rows show `  +NUM │ ` with the new-side number when known,
    /// otherwise the old-side number, otherwise `·`; side-by-side cells show
    /// ` +NUM │ `.
    MarkerThenLineNumber,
}

/// Gutter geometry shared by every row of one diff document.
///
/// Derive it once per document, for example with
/// [`DiffRowGutter::for_max_line_number`], so all rows align.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRowGutter {
    /// Cells reserved for each line number. A number with more digits shows a
    /// right-aligned `…` instead of a partial number.
    pub number_width: usize,
    /// Arrangement of numbers and marker.
    pub shape: DiffGutterShape,
}

impl DiffRowGutter {
    /// Create gutter geometry from an explicit number width.
    #[must_use]
    pub const fn new(number_width: usize, shape: DiffGutterShape) -> Self {
        Self {
            number_width,
            shape,
        }
    }

    /// Create gutter geometry wide enough for every line number up to
    /// `max_line_number`.
    #[must_use]
    pub const fn for_max_line_number(max_line_number: u32, shape: DiffGutterShape) -> Self {
        Self::new(Self::line_number_digits(max_line_number), shape)
    }

    /// Decimal digits of `line_number` (at least one).
    #[must_use]
    pub const fn line_number_digits(line_number: u32) -> usize {
        match line_number.checked_ilog10() {
            Some(log) => log as usize + 1,
            None => 1,
        }
    }

    /// Cells occupied by the gutter of a unified row.
    #[must_use]
    pub const fn unified_width(self) -> usize {
        match self.shape {
            DiffGutterShape::LineNumbersThenMarker => self.number_width * 2 + 5,
            DiffGutterShape::MarkerThenLineNumber => self.number_width + 6,
        }
    }

    /// Cells occupied by the gutter of one side-by-side cell.
    #[must_use]
    pub const fn side_width(self) -> usize {
        match self.shape {
            DiffGutterShape::LineNumbersThenMarker => self.number_width + 4,
            DiffGutterShape::MarkerThenLineNumber => self.number_width + 5,
        }
    }
}

/// How content wider than its area is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffRowOverflow {
    /// Wrap at grapheme boundaries onto continuation rows.
    ///
    /// A grapheme wider than the whole content area occupies its own row and
    /// is not painted; its bytes remain mapped to that row.
    #[default]
    Wrap,
    /// Keep one row; when content is clipped, its last cell shows `…`.
    Truncate,
    /// Keep one row showing the content from display column `offset`.
    ///
    /// Graphemes cut by either edge are omitted and their cells left blank.
    Scroll {
        /// First visible display column of the content.
        offset: usize,
    },
}

/// Column split of a side-by-side row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffSplitColumns {
    /// Split the cells left of and right of the one-cell divider evenly; the
    /// new side receives any odd cell.
    #[default]
    Even,
    /// Give the old side this many cells (clamped to the row) and the new side
    /// the remainder after the divider.
    OldWidth(usize),
}

/// Options for [`layout_diff_row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRowOptions {
    /// Gutter geometry shared by the document.
    pub gutter: DiffRowGutter,
    /// Overflow policy for content.
    pub overflow: DiffRowOverflow,
    /// Side-by-side column split.
    pub columns: DiffSplitColumns,
}

impl DiffRowOptions {
    /// Wrapping, evenly split options with the given gutter.
    #[must_use]
    pub const fn new(gutter: DiffRowGutter) -> Self {
        Self {
            gutter,
            overflow: DiffRowOverflow::Wrap,
            columns: DiffSplitColumns::Even,
        }
    }

    /// Return these options with an overflow policy.
    #[must_use]
    pub const fn with_overflow(mut self, overflow: DiffRowOverflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// Return these options with a side-by-side column split.
    #[must_use]
    pub const fn with_columns(mut self, columns: DiffSplitColumns) -> Self {
        self.columns = columns;
        self
    }
}

/// Gutter presentation chosen for a column's available width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffGutterMode {
    /// The complete gutter fits; content uses the remaining cells, possibly none.
    Full,
    /// Narrower than the gutter but at least two cells: only the marker cell
    /// is shown, followed by content.
    MarkerOnly,
    /// One cell: content only.
    Hidden,
    /// No cells.
    Empty,
}

/// Horizontal geometry of one source column of a laid-out item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRowColumn {
    /// Source side shown in this column.
    pub side: DiffSourceSide,
    /// Whether the item has content for this column. Absent side-by-side
    /// sides are blank and carry no segments.
    pub present: bool,
    /// Cells occupied by the column, gutter included.
    pub cells: Range<usize>,
    /// Cells available to content.
    pub content: Range<usize>,
    /// Gutter presentation.
    pub gutter: DiffGutterMode,
}

/// Source mapping of one row of one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRowSegment {
    /// Row within the item.
    pub row: usize,
    /// Source side.
    pub side: DiffSourceSide,
    /// Source line number, when known.
    pub line: Option<u32>,
    /// Cells occupied by the column on this row, gutter included.
    pub cells: Range<usize>,
    /// Cells available to content on this row.
    pub content: Range<usize>,
    /// Column of the first painted grapheme of `bytes`.
    ///
    /// Selection fragments for this row can be registered from
    /// `text[bytes]` painted at this column.
    pub x: usize,
    /// UTF-8 byte range of the line text shown on this row.
    ///
    /// Rows added below a shorter side-by-side cell carry an empty range at
    /// the end of that side's text.
    pub bytes: Range<usize>,
    /// Grapheme index range shown on this row.
    pub graphemes: Range<usize>,
    /// Whether this is a continuation (not the first) row of the line.
    pub continuation: bool,
    placements: Range<usize>,
}

/// Part of a column under a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowHitTarget {
    /// Gutter cells.
    Gutter,
    /// Content cells.
    Content,
}

/// Source position under a cell of a laid-out item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffRowHit {
    /// Source side.
    pub side: DiffSourceSide,
    /// Source line number, when known.
    pub line: Option<u32>,
    /// Part of the column that was hit.
    pub target: DiffRowHitTarget,
    /// Byte offset of the grapheme under the cell. Gutter hits report the
    /// start of the row; hits after the row's content report its end.
    pub byte_offset: usize,
    /// Grapheme index matching `byte_offset`.
    pub grapheme: usize,
}

/// Cells covering part of a source range on one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRowCells {
    /// Row within the item.
    pub row: usize,
    /// Covered cells.
    pub cells: Range<usize>,
}

/// One grapheme placed into a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    column: usize,
    width: usize,
    start: usize,
    end: usize,
    grapheme: usize,
}

/// Rows and source mapping of one diff item at one width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRowLayout {
    width: usize,
    rows: Vec<Line>,
    columns: Vec<DiffRowColumn>,
    segments: Vec<DiffRowSegment>,
    placements: Vec<Placement>,
}

impl DiffRowLayout {
    /// Requested width; every row is exactly this many cells.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Number of rows.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.rows.len()
    }

    /// Rendered rows.
    #[must_use]
    pub fn rows(&self) -> &[Line] {
        &self.rows
    }

    /// Take ownership of the rendered rows.
    #[must_use]
    pub fn into_rows(self) -> Vec<Line> {
        self.rows
    }

    /// Source columns, left to right.
    #[must_use]
    pub fn columns(&self) -> &[DiffRowColumn] {
        &self.columns
    }

    /// Source segments ordered by row, then column.
    #[must_use]
    pub fn segments(&self) -> &[DiffRowSegment] {
        &self.segments
    }

    /// Resolve the source position under `column` on `row`.
    ///
    /// Returns `None` for cells outside any present column (dividers, absent
    /// sides, rows beyond the item).
    #[must_use]
    pub fn hit(&self, row: usize, column: usize) -> Option<DiffRowHit> {
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.row == row && segment.cells.contains(&column))?;
        let base = DiffRowHit {
            side: segment.side,
            line: segment.line,
            target: DiffRowHitTarget::Gutter,
            byte_offset: segment.bytes.start,
            grapheme: segment.graphemes.start,
        };
        if !segment.content.contains(&column) {
            return Some(base);
        }
        let placements = &self.placements[segment.placements.clone()];
        let content = DiffRowHit {
            target: DiffRowHitTarget::Content,
            ..base
        };
        if let Some(placement) = placements.iter().find(|placement| {
            (placement.column..placement.column + placement.width).contains(&column)
        }) {
            return Some(DiffRowHit {
                byte_offset: placement.start,
                grapheme: placement.grapheme,
                ..content
            });
        }
        if placements
            .first()
            .is_some_and(|first| column < first.column)
        {
            return Some(content);
        }
        Some(DiffRowHit {
            byte_offset: segment.bytes.end,
            grapheme: segment.graphemes.end,
            ..content
        })
    }

    /// Painted cells covering `bytes` of `side`, one entry per row that shows
    /// part of the range. Graphemes overlapping the range count as covered.
    #[must_use]
    pub fn source_cells(&self, side: DiffSourceSide, bytes: Range<usize>) -> Vec<DiffRowCells> {
        let mut output = Vec::new();
        for segment in self.segments.iter().filter(|segment| segment.side == side) {
            let mut covered: Option<Range<usize>> = None;
            for placement in &self.placements[segment.placements.clone()] {
                if placement.start < bytes.end && bytes.start < placement.end {
                    let cells = placement.column..placement.column + placement.width;
                    covered = Some(match covered {
                        Some(range) => range.start.min(cells.start)..range.end.max(cells.end),
                        None => cells,
                    });
                }
            }
            if let Some(cells) = covered {
                output.push(DiffRowCells {
                    row: segment.row,
                    cells,
                });
            }
        }
        output
    }
}

/// Lay out one logical diff item at `width` cells.
///
/// Every returned row is exactly `width` cells. At width zero the item is one
/// empty row with no segments. Columns narrower than their gutter degrade as
/// described by [`DiffGutterMode`]. Rows are styled with `style`: the row
/// background of the item's kind, the marker style, muted gutter numbers, the
/// base content style patched by caller syntax ranges, and the emphasis style
/// over changed ranges.
#[must_use]
pub fn layout_diff_row(
    item: DiffRowItem<'_>,
    width: u16,
    options: DiffRowOptions,
    style: &DiffViewerStyle,
) -> DiffRowLayout {
    let width = usize::from(width);
    let mut builder = Builder::default();
    match item {
        DiffRowItem::Unified {
            kind,
            old_line,
            new_line,
            text,
        } => layout_unified(
            &mut builder,
            kind,
            old_line,
            new_line,
            text,
            width,
            options,
            style,
        ),
        DiffRowItem::SideBySide { old, new } => {
            layout_side_by_side(&mut builder, old, new, width, options, style);
        }
    }
    DiffRowLayout {
        width,
        rows: builder.rows,
        columns: builder.columns,
        segments: builder.segments,
        placements: builder.placements,
    }
}

/// Height in rows of `item` laid out at `width` cells with `options`, without producing rows.
///
/// Equals the height of the [`layout_diff_row`] result for the same item, width, and options
/// under every style, and costs one pass over the item's graphemes with no allocation, so a
/// virtualized caller can measure items it does not paint (for example lines far from the
/// viewport) cheaply.
#[must_use]
pub fn diff_row_height(item: DiffRowItem<'_>, width: u16, options: DiffRowOptions) -> usize {
    let width = usize::from(width);
    match item {
        DiffRowItem::Unified { text, .. } => {
            cell_height(text.text, width, options.gutter.unified_width(), options).max(1)
        }
        DiffRowItem::SideBySide { old, new } => {
            if width == 0 {
                return 1;
            }
            let (old_width, _, new_width) = split_widths(width, options.columns);
            let side = |cell: Option<DiffRowCell<'_>>, width| {
                cell.map_or(1, |cell| {
                    cell_height(cell.text.text, width, options.gutter.side_width(), options)
                })
            };
            side(old, old_width).max(side(new, new_width)).max(1)
        }
    }
}

/// Content rows of one present cell (zero when the cell has no cells at all).
fn cell_height(text: &str, width: usize, gutter_width: usize, options: DiffRowOptions) -> usize {
    let (mode, prefix) = gutter_mode(width, gutter_width);
    if mode == DiffGutterMode::Empty {
        return 0;
    }
    let content_width = width - prefix;
    match options.overflow {
        DiffRowOverflow::Wrap if content_width > 0 => {
            let mut rows = 1;
            wrap_walk(text, content_width, |step| {
                if matches!(step, WrapStep::Break { .. }) {
                    rows += 1;
                }
            });
            rows
        }
        _ => 1,
    }
}

/// One step of wrapping a text at grapheme boundaries.
#[derive(Clone, Copy)]
enum WrapStep {
    /// A new row starts at this grapheme.
    Break { index: usize, start: usize },
    /// A grapheme lands at `column` of the current row. Zero-width graphemes have zero
    /// `cells`; a grapheme wider than the whole row takes a row of its own and is not
    /// painted (`cells` exceeds the width).
    Place {
        column: usize,
        cells: usize,
        start: usize,
        end: usize,
        index: usize,
    },
}

/// Wrap `text` into rows of `width` cells, reporting each step. The single source of wrap
/// geometry for both [`layout_diff_row`] and [`diff_row_height`].
fn wrap_walk(text: &str, width: usize, mut step: impl FnMut(WrapStep)) {
    let mut column = 0;
    for (index, start, end, cells) in graphemes(text) {
        if cells > 0 && column > 0 && column + cells > width {
            step(WrapStep::Break { index, start });
            column = 0;
        }
        step(WrapStep::Place {
            column,
            cells,
            start,
            end,
            index,
        });
        if cells > 0 {
            column = if cells > width { width } else { column + cells };
        }
    }
}

#[derive(Default)]
struct Builder {
    rows: Vec<Line>,
    columns: Vec<DiffRowColumn>,
    segments: Vec<DiffRowSegment>,
    placements: Vec<Placement>,
}

#[allow(clippy::too_many_arguments)] // Private adapter of one public item variant.
fn layout_unified(
    builder: &mut Builder,
    kind: DiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: DiffRowText<'_>,
    width: usize,
    options: DiffRowOptions,
    style: &DiffViewerStyle,
) {
    let (side, line) = unified_source(kind, old_line, new_line);
    let numbers = GutterNumbers::Unified { old_line, new_line };
    let cell = CellSpec {
        kind,
        side,
        line,
        numbers,
        text,
        width,
        gutter: options.gutter,
        unified: true,
        overflow: options.overflow,
        pad: style.row(kind),
    };
    let laid = layout_cell(&cell, 0, style, &mut builder.placements);
    builder.columns.push(laid.column.clone());
    if laid.rows.is_empty() {
        builder.rows.push(Line::default());
        return;
    }
    for (row, cell_row) in laid.rows.into_iter().enumerate() {
        if let Some(mut segment) = cell_row.segment {
            segment.row = row;
            builder.segments.push(segment);
        }
        builder.rows.push(Line::from_spans(cell_row.spans));
    }
}

fn layout_side_by_side(
    builder: &mut Builder,
    old: Option<DiffRowCell<'_>>,
    new: Option<DiffRowCell<'_>>,
    width: usize,
    options: DiffRowOptions,
    style: &DiffViewerStyle,
) {
    let (old_width, divider, new_width) = split_widths(width, options.columns);
    let old_laid = side_cell(
        old,
        DiffSourceSide::Old,
        old_width,
        0,
        options,
        style,
        builder,
    );
    let new_x = old_width + divider;
    let new_laid = side_cell(
        new,
        DiffSourceSide::New,
        new_width,
        new_x,
        options,
        style,
        builder,
    );
    builder.columns.push(old_laid.column.clone());
    builder.columns.push(new_laid.column.clone());
    if width == 0 {
        builder.rows.push(Line::default());
        return;
    }
    let height = old_laid.rows.len().max(new_laid.rows.len()).max(1);
    let mut old_rows = old_laid.rows.into_iter();
    let mut new_rows = new_laid.rows.into_iter();
    let old_filler = old_laid.filler;
    let new_filler = new_laid.filler;
    for row in 0..height {
        let mut spans = Vec::new();
        for (rows, filler, divider_after) in [
            (&mut old_rows, &old_filler, divider > 0),
            (&mut new_rows, &new_filler, false),
        ] {
            let cell_row = rows.next().unwrap_or_else(|| filler.next_row());
            spans.extend(cell_row.spans);
            if let Some(mut segment) = cell_row.segment {
                segment.row = row;
                builder.segments.push(segment);
            }
            if divider_after {
                spans.push(Span::styled("│", style.muted));
            }
        }
        builder.rows.push(Line::from_spans(spans));
    }
}

fn side_cell(
    cell: Option<DiffRowCell<'_>>,
    side: DiffSourceSide,
    width: usize,
    x: usize,
    options: DiffRowOptions,
    style: &DiffViewerStyle,
    builder: &mut Builder,
) -> LaidCell {
    let Some(cell) = cell else {
        return LaidCell::absent(side, x, width, style.text);
    };
    let spec = CellSpec {
        kind: cell.kind,
        side,
        line: cell.line,
        numbers: GutterNumbers::Side(cell.line),
        text: cell.text,
        width,
        gutter: options.gutter,
        unified: false,
        overflow: options.overflow,
        pad: style.row(cell.kind).patch(style.line(cell.kind).2),
    };
    layout_cell(&spec, x, style, &mut builder.placements)
}

const fn split_widths(width: usize, columns: DiffSplitColumns) -> (usize, usize, usize) {
    let divider = if width >= 3 { 1 } else { 0 };
    let cells = width - divider;
    let old = match columns {
        DiffSplitColumns::Even => cells / 2,
        DiffSplitColumns::OldWidth(old) => {
            if old < cells {
                old
            } else {
                cells
            }
        }
    };
    (old, divider, cells - old)
}

const fn unified_source(
    kind: DiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
) -> (DiffSourceSide, Option<u32>) {
    match kind {
        DiffLineKind::Removed => (DiffSourceSide::Old, old_line),
        _ if new_line.is_none() && old_line.is_some() => (DiffSourceSide::Old, old_line),
        _ => (DiffSourceSide::New, new_line),
    }
}

#[derive(Clone, Copy)]
enum GutterNumbers {
    Unified {
        old_line: Option<u32>,
        new_line: Option<u32>,
    },
    Side(Option<u32>),
}

struct CellSpec<'a> {
    kind: DiffLineKind,
    side: DiffSourceSide,
    line: Option<u32>,
    numbers: GutterNumbers,
    text: DiffRowText<'a>,
    width: usize,
    gutter: DiffRowGutter,
    unified: bool,
    overflow: DiffRowOverflow,
    pad: Style,
}

struct CellRow {
    spans: Vec<Span>,
    segment: Option<DiffRowSegment>,
}

/// Rows appended below a shorter side-by-side cell.
struct Filler {
    spans: Vec<Span>,
    segment: Option<DiffRowSegment>,
}

impl Filler {
    fn next_row(&self) -> CellRow {
        CellRow {
            spans: self.spans.clone(),
            segment: self.segment.clone(),
        }
    }
}

struct LaidCell {
    column: DiffRowColumn,
    rows: Vec<CellRow>,
    filler: Filler,
}

impl LaidCell {
    fn absent(side: DiffSourceSide, x: usize, width: usize, style: Style) -> Self {
        let spans = blank_spans(width, style);
        Self {
            column: DiffRowColumn {
                side,
                present: false,
                cells: x..x + width,
                content: x + width..x + width,
                gutter: if width == 0 {
                    DiffGutterMode::Empty
                } else {
                    DiffGutterMode::Hidden
                },
            },
            rows: vec![CellRow {
                spans: spans.clone(),
                segment: None,
            }],
            filler: Filler {
                spans,
                segment: None,
            },
        }
    }
}

fn blank_spans(width: usize, style: Style) -> Vec<Span> {
    if width == 0 {
        Vec::new()
    } else {
        vec![Span::styled(" ".repeat(width), style)]
    }
}

const fn gutter_mode(width: usize, gutter_width: usize) -> (DiffGutterMode, usize) {
    if width == 0 {
        (DiffGutterMode::Empty, 0)
    } else if width >= gutter_width {
        (DiffGutterMode::Full, gutter_width)
    } else if width >= 2 {
        (DiffGutterMode::MarkerOnly, 1)
    } else {
        (DiffGutterMode::Hidden, 0)
    }
}

fn layout_cell(
    spec: &CellSpec<'_>,
    x: usize,
    style: &DiffViewerStyle,
    placements: &mut Vec<Placement>,
) -> LaidCell {
    let gutter_width = if spec.unified {
        spec.gutter.unified_width()
    } else {
        spec.gutter.side_width()
    };
    let (mode, prefix) = gutter_mode(spec.width, gutter_width);
    let content_width = spec.width - prefix;
    let column = DiffRowColumn {
        side: spec.side,
        present: true,
        cells: x..x + spec.width,
        content: x + prefix..x + spec.width,
        gutter: mode,
    };
    if mode == DiffGutterMode::Empty {
        return LaidCell {
            column,
            rows: Vec::new(),
            filler: Filler {
                spans: Vec::new(),
                segment: None,
            },
        };
    }
    let content_rows = place_content(spec.text.text, content_width, spec.overflow);
    let styles = ContentStyles::new(spec, style);
    let mut rows = Vec::with_capacity(content_rows.len());
    for (index, content_row) in content_rows.iter().enumerate() {
        let mut spans = gutter_spans(spec, mode, index == 0, style);
        let first_placement = placements.len();
        styles.push_content_spans(spec.text, content_row, content_width, &mut spans);
        placements.extend(content_row.placements.iter().map(|placement| Placement {
            column: placement.column + x + prefix,
            ..*placement
        }));
        let painted_x = content_row
            .placements
            .first()
            .map_or(x + prefix, |placement| placement.column + x + prefix);
        rows.push(CellRow {
            spans,
            segment: Some(DiffRowSegment {
                row: 0,
                side: spec.side,
                line: spec.line,
                cells: column.cells.clone(),
                content: column.content.clone(),
                x: painted_x,
                bytes: content_row.bytes.clone(),
                graphemes: content_row.graphemes.clone(),
                continuation: index > 0,
                placements: first_placement..placements.len(),
            }),
        });
    }
    let end_bytes = content_rows
        .last()
        .map_or(0..0, |row| row.bytes.end..row.bytes.end);
    let end_graphemes = content_rows
        .last()
        .map_or(0..0, |row| row.graphemes.end..row.graphemes.end);
    let mut filler_spans = gutter_spans(spec, mode, false, style);
    filler_spans.extend(blank_spans(content_width, spec.pad));
    let filler = Filler {
        spans: filler_spans,
        segment: Some(DiffRowSegment {
            row: 0,
            side: spec.side,
            line: spec.line,
            cells: column.cells.clone(),
            content: column.content.clone(),
            x: x + prefix,
            bytes: end_bytes,
            graphemes: end_graphemes,
            continuation: true,
            placements: 0..0,
        }),
    };
    LaidCell {
        column,
        rows,
        filler,
    }
}

fn gutter_spans(
    spec: &CellSpec<'_>,
    mode: DiffGutterMode,
    first: bool,
    style: &DiffViewerStyle,
) -> Vec<Span> {
    let (sign, sign_style, _) = style.line(spec.kind);
    let row_style = style.row(spec.kind);
    let gutter = row_style.patch(style.muted);
    let marker_style = row_style.patch(sign_style.add_modifier(Modifier::BOLD));
    let marker = if first { sign } else { " " };
    let number_width = spec.gutter.number_width;
    let number = |line: Option<u32>, fallback: &str| {
        if first {
            format_number(line, number_width, fallback)
        } else {
            " ".repeat(number_width)
        }
    };
    match mode {
        DiffGutterMode::Empty | DiffGutterMode::Hidden => Vec::new(),
        DiffGutterMode::MarkerOnly => vec![Span::styled(marker, marker_style)],
        DiffGutterMode::Full => match (spec.gutter.shape, spec.numbers) {
            (
                DiffGutterShape::MarkerThenLineNumber,
                GutterNumbers::Unified { old_line, new_line },
            ) => vec![
                Span::styled("  ", gutter),
                Span::styled(marker, if first { marker_style } else { gutter }),
                Span::styled(number(new_line.or(old_line), "·"), gutter),
                Span::styled(" │ ", gutter),
            ],
            (DiffGutterShape::MarkerThenLineNumber, GutterNumbers::Side(line)) => vec![
                Span::styled(" ", gutter),
                Span::styled(marker, marker_style),
                Span::styled(number(line, ""), gutter),
                Span::styled(" │ ", gutter),
            ],
            (
                DiffGutterShape::LineNumbersThenMarker,
                GutterNumbers::Unified { old_line, new_line },
            ) => vec![
                Span::styled(" ", gutter),
                Span::styled(number(old_line, ""), gutter),
                Span::styled(" ", gutter),
                Span::styled(number(new_line, ""), gutter),
                Span::styled(" ", gutter),
                Span::styled(marker, marker_style),
                Span::styled(" ", gutter),
            ],
            (DiffGutterShape::LineNumbersThenMarker, GutterNumbers::Side(line)) => vec![
                Span::styled(" ", gutter),
                Span::styled(number(line, ""), gutter),
                Span::styled(" ", gutter),
                Span::styled(marker, marker_style),
                Span::styled(" ", gutter),
            ],
        },
    }
}

/// Right-align a line number (or `fallback`) in `width` cells. A number that
/// does not fit shows a right-aligned `…` rather than a partial number.
fn format_number(line: Option<u32>, width: usize, fallback: &str) -> String {
    let text = line.map_or_else(|| fallback.to_owned(), |line| line.to_string());
    let text_width = bmux_tui::text_width::display_width(&text);
    if text_width <= width {
        format!("{}{text}", " ".repeat(width - text_width))
    } else if width == 0 {
        String::new()
    } else {
        format!("{}…", " ".repeat(width - 1))
    }
}

/// Graphemes placed on one content row, before translation to item columns.
struct ContentRow {
    placements: Vec<Placement>,
    bytes: Range<usize>,
    graphemes: Range<usize>,
    /// Column and style-source byte of a trailing truncation marker.
    ellipsis: Option<(usize, usize)>,
}

impl ContentRow {
    const fn new(byte: usize, grapheme: usize) -> Self {
        Self {
            placements: Vec::new(),
            bytes: byte..byte,
            graphemes: grapheme..grapheme,
            ellipsis: None,
        }
    }

    fn place(&mut self, column: usize, width: usize, start: usize, end: usize, grapheme: usize) {
        self.placements.push(Placement {
            column,
            width,
            start,
            end,
            grapheme,
        });
        self.bytes.end = end;
        self.graphemes.end = grapheme + 1;
    }

    /// Attach a zero-width grapheme to the preceding placement on this row.
    fn attach(&mut self, end: usize, grapheme: usize) {
        if let Some(last) = self.placements.last_mut() {
            last.end = end;
        }
        self.bytes.end = end;
        self.graphemes.end = grapheme + 1;
    }
}

fn place_content(text: &str, width: usize, overflow: DiffRowOverflow) -> Vec<ContentRow> {
    if width == 0 {
        let mut row = ContentRow::new(0, 0);
        row.bytes.end = text.len();
        row.graphemes.end = text.graphemes(true).count();
        return vec![row];
    }
    match overflow {
        DiffRowOverflow::Wrap => place_wrapped(text, width),
        DiffRowOverflow::Truncate => vec![place_truncated(text, width)],
        DiffRowOverflow::Scroll { offset } => vec![place_scrolled(text, width, offset)],
    }
}

fn graphemes(text: &str) -> impl Iterator<Item = (usize, usize, usize, usize)> + '_ {
    text.grapheme_indices(true)
        .enumerate()
        .map(|(index, (start, grapheme))| {
            (
                index,
                start,
                start + grapheme.len(),
                usize::from(painted_grapheme_width(grapheme)),
            )
        })
}

fn place_wrapped(text: &str, width: usize) -> Vec<ContentRow> {
    let mut rows = Vec::new();
    let mut row = ContentRow::new(0, 0);
    // Start of zero-width bytes that precede the row's first placement.
    let mut leading: Option<usize> = None;
    wrap_walk(text, width, |step| match step {
        WrapStep::Break { index, start } => {
            rows.push(std::mem::replace(&mut row, ContentRow::new(start, index)));
            leading = None;
        }
        WrapStep::Place {
            cells: 0,
            start,
            end,
            index,
            ..
        } => {
            if row.placements.is_empty() {
                leading.get_or_insert(start);
                row.bytes.end = end;
                row.graphemes.end = index + 1;
            } else {
                row.attach(end, index);
            }
        }
        WrapStep::Place {
            cells, end, index, ..
        } if cells > width => {
            // An indivisible grapheme wider than the area: reserve its own
            // unpainted row so content never exceeds the area.
            row.bytes.end = end;
            row.graphemes.end = index + 1;
        }
        WrapStep::Place {
            column,
            cells,
            start,
            end,
            index,
        } => row.place(column, cells, leading.take().unwrap_or(start), end, index),
    });
    rows.push(row);
    rows
}

fn place_truncated(text: &str, width: usize) -> ContentRow {
    let total = graphemes(text).map(|(_, _, _, cells)| cells).sum::<usize>();
    if total <= width {
        let mut rows = place_wrapped(text, width);
        return rows.swap_remove(0);
    }
    let body = width - 1;
    let mut row = ContentRow::new(0, 0);
    let mut column = 0;
    let mut leading: Option<usize> = None;
    for (index, start, end, cells) in graphemes(text) {
        if cells == 0 {
            if row.placements.is_empty() {
                leading.get_or_insert(start);
                row.bytes.end = end;
                row.graphemes.end = index + 1;
            } else {
                row.attach(end, index);
            }
            continue;
        }
        if column + cells > body {
            row.ellipsis = Some((column, start));
            break;
        }
        row.place(column, cells, leading.take().unwrap_or(start), end, index);
        column += cells;
    }
    row
}

fn place_scrolled(text: &str, width: usize, offset: usize) -> ContentRow {
    let mut row: Option<ContentRow> = None;
    let mut position = (text.len(), text.graphemes(true).count());
    let mut absolute = 0;
    let mut previous_placed = false;
    for (index, start, end, cells) in graphemes(text) {
        if cells == 0 {
            if previous_placed && let Some(row) = row.as_mut() {
                row.attach(end, index);
            }
            continue;
        }
        if absolute < offset {
            absolute += cells;
            previous_placed = false;
            continue;
        }
        if absolute + cells > offset + width {
            if row.is_none() {
                position = (start, index);
            }
            break;
        }
        let row = row.get_or_insert_with(|| ContentRow::new(start, index));
        row.place(absolute - offset, cells, start, end, index);
        absolute += cells;
        previous_placed = true;
    }
    row.unwrap_or_else(|| ContentRow::new(position.0, position.1))
}

/// Resolved styles for one cell's content.
struct ContentStyles {
    base: Style,
    emphasis: Style,
    pad: Style,
}

impl ContentStyles {
    const fn new(spec: &CellSpec<'_>, style: &DiffViewerStyle) -> Self {
        let (_, _, body_style) = style.line(spec.kind);
        Self {
            base: style.row(spec.kind).patch(body_style),
            emphasis: style.emphasis(spec.kind),
            pad: spec.pad,
        }
    }

    /// Style of the grapheme at `start..end`. Both range lists are sorted and
    /// disjoint, so each lookup is a binary search.
    fn grapheme_style(&self, text: DiffRowText<'_>, start: usize, end: usize) -> Style {
        let mut style = self.base;
        let syntax = text.syntax.partition_point(|range| range.end <= start);
        if let Some(range) = text.syntax.get(syntax).filter(|range| range.start <= start) {
            style = style.patch(range.style);
        }
        let changed = text
            .changed_ranges
            .partition_point(|range| range.end <= start);
        if text
            .changed_ranges
            .get(changed)
            .is_some_and(|range| range.start < end)
        {
            style = style.patch(self.emphasis);
        }
        style
    }

    /// Append exactly `width` cells of content spans for `row`.
    fn push_content_spans(
        &self,
        text: DiffRowText<'_>,
        row: &ContentRow,
        width: usize,
        spans: &mut Vec<Span>,
    ) {
        let mut output = SpanRun::new(spans, self.pad);
        let mut column = 0;
        for placement in &row.placements {
            if placement.column > column {
                output.blank(placement.column - column, self.pad);
            }
            let style = self.grapheme_style(text, placement.start, placement.end);
            output.push(&text.text[placement.start..placement.end], style);
            column = placement.column + placement.width;
        }
        if let Some((ellipsis_column, source)) = row.ellipsis {
            if ellipsis_column > column {
                output.blank(ellipsis_column - column, self.pad);
            }
            let end = text.text[source..]
                .graphemes(true)
                .next()
                .map_or(source, |grapheme| source + grapheme.len());
            output.push("…", self.grapheme_style(text, source, end));
            column = ellipsis_column + 1;
        }
        if width > column {
            output.blank(width - column, self.pad);
        }
        output.finish();
    }
}

/// Appends text to spans, merging neighbours that share a style.
struct SpanRun<'a> {
    spans: &'a mut Vec<Span>,
    pending: String,
    style: Style,
}

impl<'a> SpanRun<'a> {
    const fn new(spans: &'a mut Vec<Span>, style: Style) -> Self {
        Self {
            spans,
            pending: String::new(),
            style,
        }
    }

    fn push(&mut self, content: &str, style: Style) {
        if style != self.style {
            self.flush();
            self.style = style;
        }
        self.pending.push_str(content);
    }

    fn blank(&mut self, cells: usize, style: Style) {
        if style != self.style {
            self.flush();
            self.style = style;
        }
        self.pending.extend(std::iter::repeat_n(' ', cells));
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.spans
                .push(Span::styled(std::mem::take(&mut self.pending), self.style));
        }
    }

    fn finish(mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests;
