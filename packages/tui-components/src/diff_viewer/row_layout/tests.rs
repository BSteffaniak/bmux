//! Behavior tests for row-level diff layout.

use super::*;
use crate::diff_viewer::DiffViewerStyle;
use bmux_tui::buffer::Buffer;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::Color;
use bmux_tui::selection::painted_text_fragments;

const TEXTS: &[&str] = &[
    "",
    "plain ascii content that is long enough to wrap",
    "東京都の天気は晴れです。明日も晴れ",
    "family 👨‍👩‍👧‍👦 faces 😀😃 flags 🇯🇵",
    "e\u{301}a\u{308}o\u{302} combining e\u{301}\u{302} marks",
    "zero\u{200b}width\u{200b}\u{200b}joins",
    "\u{200b}leading zero width",
    "tab\tseparated\tcontrol",
    "界",
];

fn style() -> DiffViewerStyle {
    DiffViewerStyle::default()
}

fn painted_width(line: &Line) -> usize {
    line.spans
        .iter()
        .flat_map(|span| span.content.graphemes(true))
        .map(|grapheme| usize::from(painted_grapheme_width(grapheme)))
        .sum()
}

fn paint(layout: &DiffRowLayout) -> Buffer {
    let width = u16::try_from(layout.width() + 4).unwrap();
    let height = u16::try_from(layout.height()).unwrap();
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    for (row, line) in layout.rows().iter().enumerate() {
        let y = u16::try_from(row).unwrap();
        buffer.write_line(Rect::new(0, y, width, 1), line);
    }
    buffer
}

fn symbol(buffer: &Buffer, column: usize, row: usize) -> String {
    buffer
        .get(Point::new(
            u16::try_from(column).unwrap(),
            u16::try_from(row).unwrap(),
        ))
        .map(|cell| cell.symbol.clone())
        .unwrap_or_default()
}

fn row_text(buffer: &Buffer, columns: Range<usize>, row: usize) -> String {
    columns.map(|column| symbol(buffer, column, row)).collect()
}

const fn unified(kind: DiffLineKind, text: DiffRowText<'_>) -> DiffRowItem<'_> {
    DiffRowItem::Unified {
        kind,
        old_line: Some(41),
        new_line: Some(42),
        text,
    }
}

const fn options(shape: DiffGutterShape, overflow: DiffRowOverflow) -> DiffRowOptions {
    DiffRowOptions::new(DiffRowGutter::for_max_line_number(9_999, shape)).with_overflow(overflow)
}

const SHAPES: [DiffGutterShape; 2] = [
    DiffGutterShape::LineNumbersThenMarker,
    DiffGutterShape::MarkerThenLineNumber,
];

const OVERFLOWS: [DiffRowOverflow; 4] = [
    DiffRowOverflow::Wrap,
    DiffRowOverflow::Truncate,
    DiffRowOverflow::Scroll { offset: 0 },
    DiffRowOverflow::Scroll { offset: 3 },
];

const fn side_by_side<'a>(old: &'a str, new: &'a str) -> DiffRowItem<'a> {
    DiffRowItem::SideBySide {
        old: Some(DiffRowCell::new(
            DiffLineKind::Removed,
            Some(7),
            DiffRowText::plain(old),
        )),
        new: Some(DiffRowCell::new(
            DiffLineKind::Added,
            Some(8),
            DiffRowText::plain(new),
        )),
    }
}

/// A laid-out item with the old-side and new-side texts it was built from.
struct Laid<'a> {
    layout: DiffRowLayout,
    old: &'a str,
    new: &'a str,
}

impl Laid<'_> {
    const fn source(&self, side: DiffSourceSide) -> &str {
        match side {
            DiffSourceSide::Old => self.old,
            DiffSourceSide::New => self.new,
        }
    }
}

/// Every item shape the layout produces, at one width.
fn all_layouts(text: &str, width: u16) -> Vec<Laid<'_>> {
    let mut output = Vec::new();
    for shape in SHAPES {
        for overflow in OVERFLOWS {
            let options = options(shape, overflow);
            for kind in [
                DiffLineKind::Added,
                DiffLineKind::Removed,
                DiffLineKind::Context,
            ] {
                output.push(Laid {
                    layout: layout_diff_row(
                        unified(kind, DiffRowText::plain(text)),
                        width,
                        options,
                        &style(),
                    ),
                    old: text,
                    new: text,
                });
            }
            output.push(Laid {
                layout: layout_diff_row(side_by_side(text, "short"), width, options, &style()),
                old: text,
                new: "short",
            });
            output.push(Laid {
                layout: layout_diff_row(
                    DiffRowItem::SideBySide {
                        old: None,
                        new: Some(DiffRowCell::new(
                            DiffLineKind::Added,
                            Some(1),
                            DiffRowText::plain(text),
                        )),
                    },
                    width,
                    options,
                    &style(),
                ),
                old: "",
                new: text,
            });
        }
    }
    output
}

#[test]
fn every_row_is_exactly_the_requested_width() {
    for text in TEXTS {
        for width in 0..48_u16 {
            for laid in all_layouts(text, width) {
                assert!(laid.layout.height() >= 1, "{text:?} {width}");
                for row in laid.layout.rows() {
                    assert_eq!(
                        painted_width(row),
                        usize::from(width),
                        "{text:?} width={width} row={row:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn zero_width_is_one_empty_row_without_hit_regions() {
    for laid in all_layouts("content", 0) {
        assert_eq!(laid.layout.height(), 1);
        assert_eq!(laid.layout.rows()[0].spans, []);
        assert_eq!(laid.layout.segments(), []);
        assert_eq!(laid.layout.hit(0, 0), None);
    }
}

#[test]
fn wrapped_segments_cover_the_source_exactly_once() {
    for text in TEXTS {
        for width in 1..48_u16 {
            for shape in SHAPES {
                let layout = layout_diff_row(
                    unified(DiffLineKind::Added, DiffRowText::plain(text)),
                    width,
                    options(shape, DiffRowOverflow::Wrap),
                    &style(),
                );
                let mut byte = 0;
                let mut grapheme = 0;
                for (row, segment) in layout.segments().iter().enumerate() {
                    assert_eq!(segment.row, row);
                    assert_eq!(segment.bytes.start, byte, "{text:?} {width}");
                    assert_eq!(segment.graphemes.start, grapheme);
                    assert_eq!(segment.continuation, row > 0);
                    assert_eq!(segment.side, DiffSourceSide::New);
                    assert_eq!(segment.line, Some(42));
                    byte = segment.bytes.end;
                    grapheme = segment.graphemes.end;
                }
                assert_eq!(byte, text.len(), "{text:?} {width}");
                assert_eq!(grapheme, text.graphemes(true).count());
                assert_eq!(layout.segments().len(), layout.height());
            }
        }
    }
}

/// Selection fragments registered from a segment, hit testing, and painted
/// cells must all describe the same cells.
#[test]
fn hit_regions_selection_fragments_and_paint_agree() {
    for text in TEXTS {
        for width in [0_u16, 1, 2, 5, 9, 14, 17, 23, 40] {
            for laid in all_layouts(text, width) {
                let layout = &laid.layout;
                let buffer = paint(layout);
                for segment in layout.segments() {
                    let source = laid.source(segment.side);
                    assert_segment_agrees(layout, &buffer, segment, source);
                }
            }
        }
    }
}

fn assert_segment_agrees(
    layout: &DiffRowLayout,
    buffer: &Buffer,
    segment: &DiffRowSegment,
    source: &str,
) {
    let fragments = painted_text_fragments(
        "scope",
        "content",
        Rect::new(
            u16::try_from(segment.x).unwrap(),
            u16::try_from(segment.row).unwrap(),
            u16::try_from(segment.content.end.saturating_sub(segment.x)).unwrap(),
            1,
        ),
        0,
        &source[segment.bytes.clone()],
        segment.bytes.start,
        0,
    );
    let mut painted_any = false;
    for fragment in &fragments {
        let Some(grapheme) = source[fragment.source_range.clone()]
            .graphemes(true)
            .find(|grapheme| painted_grapheme_width(grapheme) > 0)
        else {
            continue;
        };
        painted_any = true;
        let x = usize::from(fragment.area.x);
        let cell = symbol(buffer, x, segment.row);
        if grapheme.chars().any(char::is_control) {
            assert_eq!(cell, "\u{fffd}");
        } else {
            assert_eq!(cell, grapheme, "{source:?} {segment:?}");
        }
        let cells = x..x + usize::from(fragment.area.width);
        for column in cells.clone() {
            let hit = layout.hit(segment.row, column).expect("content hit");
            assert_eq!(hit.target, DiffRowHitTarget::Content);
            assert_eq!(hit.side, segment.side);
            assert_eq!(hit.line, segment.line);
            assert_eq!(
                hit.byte_offset, fragment.source_range.start,
                "{source:?} column={column}"
            );
        }
        assert!(
            layout
                .source_cells(segment.side, fragment.source_range.clone())
                .iter()
                .any(|covered| covered.row == segment.row && covered.cells == cells),
            "{source:?} {segment:?}"
        );
    }
    if !painted_any {
        assert!(
            layout
                .source_cells(segment.side, segment.bytes.clone())
                .iter()
                .all(|covered| covered.row != segment.row)
        );
    }
    for column in segment.cells.start..segment.content.start {
        let hit = layout.hit(segment.row, column).expect("gutter hit");
        assert_eq!(hit.target, DiffRowHitTarget::Gutter);
        assert_eq!(hit.byte_offset, segment.bytes.start);
    }
}

#[test]
fn gutter_is_derived_once_and_aligns_every_row() {
    let gutter = DiffRowGutter::for_max_line_number(12_345, DiffGutterShape::LineNumbersThenMarker);
    assert_eq!(gutter.number_width, 5);
    for (old_line, new_line) in [(Some(1), Some(1)), (None, Some(99)), (Some(12_345), None)] {
        let layout = layout_diff_row(
            DiffRowItem::Unified {
                kind: DiffLineKind::Context,
                old_line,
                new_line,
                text: DiffRowText::plain("body"),
            },
            30,
            DiffRowOptions::new(gutter),
            &style(),
        );
        let buffer = paint(&layout);
        assert_eq!(layout.columns()[0].content.start, gutter.unified_width());
        assert_eq!(symbol(&buffer, gutter.unified_width(), 0), "b");
        let old = old_line.map_or_else(String::new, |line| line.to_string());
        let new = new_line.map_or_else(String::new, |line| line.to_string());
        assert_eq!(row_text(&buffer, 1..6, 0), format!("{old:>5}"));
        assert_eq!(row_text(&buffer, 7..12, 0), format!("{new:>5}"));
    }
}

#[test]
fn compact_gutter_shows_one_number_and_a_placeholder() {
    let gutter = DiffRowGutter::new(3, DiffGutterShape::MarkerThenLineNumber);
    let render = |old_line, new_line| {
        let layout = layout_diff_row(
            DiffRowItem::Unified {
                kind: DiffLineKind::Removed,
                old_line,
                new_line,
                text: DiffRowText::plain("x"),
            },
            20,
            DiffRowOptions::new(gutter),
            &style(),
        );
        row_text(&paint(&layout), 0..gutter.unified_width(), 0)
    };
    assert_eq!(render(Some(5), None), "  -  5 │ ");
    assert_eq!(render(Some(5), Some(9)), "  -  9 │ ");
    assert_eq!(render(None, None), "  -  · │ ");
}

#[test]
fn oversized_numbers_never_render_partially() {
    let gutter = DiffRowGutter::new(2, DiffGutterShape::LineNumbersThenMarker);
    let layout = layout_diff_row(
        DiffRowItem::Unified {
            kind: DiffLineKind::Added,
            old_line: None,
            new_line: Some(12_345),
            text: DiffRowText::plain("x"),
        },
        20,
        DiffRowOptions::new(gutter),
        &style(),
    );
    let row = row_text(&paint(&layout), 0..20, 0);
    assert!(!row.contains('1') && !row.contains('5'), "{row:?}");
    assert!(row.starts_with("     … + x"), "{row:?}");
}

#[test]
fn narrow_columns_degrade_explicitly() {
    let options = options(
        DiffGutterShape::LineNumbersThenMarker,
        DiffRowOverflow::Wrap,
    );
    let gutter = options.gutter.unified_width();
    let at = |width: usize| {
        layout_diff_row(
            unified(DiffLineKind::Added, DiffRowText::plain("abc")),
            u16::try_from(width).unwrap(),
            options,
            &style(),
        )
    };
    let full = at(gutter);
    assert_eq!(full.columns()[0].gutter, DiffGutterMode::Full);
    assert!(full.columns()[0].content.is_empty());
    assert_eq!(
        full.segments()[0].bytes,
        0..3,
        "unpainted content stays mapped"
    );

    let marker = at(gutter - 1);
    assert_eq!(marker.columns()[0].gutter, DiffGutterMode::MarkerOnly);
    assert_eq!(marker.columns()[0].content, 1..gutter - 1);
    let buffer = paint(&marker);
    assert_eq!(symbol(&buffer, 0, 0), "+");
    assert_eq!(symbol(&buffer, 1, 0), "a");

    let hidden = at(1);
    assert_eq!(hidden.columns()[0].gutter, DiffGutterMode::Hidden);
    assert_eq!(hidden.height(), 3);
    assert_eq!(symbol(&paint(&hidden), 0, 2), "c");

    assert_eq!(at(0).columns()[0].gutter, DiffGutterMode::Empty);
}

#[test]
fn wide_graphemes_never_overflow_a_one_cell_area() {
    let layout = layout_diff_row(
        unified(DiffLineKind::Context, DiffRowText::plain("a界b")),
        1,
        options(
            DiffGutterShape::LineNumbersThenMarker,
            DiffRowOverflow::Wrap,
        ),
        &style(),
    );
    assert_eq!(layout.height(), 3);
    let buffer = paint(&layout);
    assert_eq!(symbol(&buffer, 0, 0), "a");
    assert_eq!(symbol(&buffer, 0, 1), " ", "unpainted reservation");
    assert_eq!(symbol(&buffer, 0, 2), "b");
    assert_eq!(layout.segments()[1].bytes, 1..4);
    assert_eq!(layout.hit(1, 0).map(|hit| hit.byte_offset), Some(4));
    assert_eq!(layout.source_cells(DiffSourceSide::New, 1..4), []);
}

#[test]
fn truncation_marks_clipped_content_and_maps_only_visible_bytes() {
    let options = options(
        DiffGutterShape::LineNumbersThenMarker,
        DiffRowOverflow::Truncate,
    );
    let content = options.gutter.unified_width();
    let layout = layout_diff_row(
        unified(DiffLineKind::Added, DiffRowText::plain("東京abcdef")),
        u16::try_from(content + 6).unwrap(),
        options,
        &style(),
    );
    assert_eq!(layout.height(), 1);
    let buffer = paint(&layout);
    let shown = row_text(&buffer, content..content + 6, 0).replace(' ', "");
    assert_eq!(shown, "東京a…");
    assert_eq!(layout.segments()[0].bytes, 0..7);
    assert_eq!(
        layout.hit(0, content + 5).map(|hit| hit.byte_offset),
        Some(7)
    );

    let fits = layout_diff_row(
        unified(DiffLineKind::Added, DiffRowText::plain("東京")),
        u16::try_from(content + 4).unwrap(),
        options,
        &style(),
    );
    let shown = row_text(&paint(&fits), content..content + 4, 0).replace(' ', "");
    assert_eq!(shown, "東京");
}

#[test]
fn horizontal_scroll_blanks_cut_graphemes_and_shifts_mapping() {
    let gutter = DiffRowGutter::new(1, DiffGutterShape::LineNumbersThenMarker);
    let content = gutter.unified_width();
    let layout = layout_diff_row(
        unified(DiffLineKind::Context, DiffRowText::plain("東京ab")),
        u16::try_from(content + 4).unwrap(),
        DiffRowOptions::new(gutter).with_overflow(DiffRowOverflow::Scroll { offset: 1 }),
        &style(),
    );
    let segment = &layout.segments()[0];
    assert_eq!(segment.bytes, 3..7);
    assert_eq!(segment.x, content + 1);
    let buffer = paint(&layout);
    assert_eq!(symbol(&buffer, content, 0), " ");
    assert_eq!(symbol(&buffer, content + 1, 0), "京");
    assert_eq!(symbol(&buffer, content + 3, 0), "a");
    assert_eq!(layout.hit(0, content).map(|hit| hit.byte_offset), Some(3));

    let past_end = layout_diff_row(
        unified(DiffLineKind::Context, DiffRowText::plain("ab")),
        u16::try_from(content + 4).unwrap(),
        DiffRowOptions::new(gutter).with_overflow(DiffRowOverflow::Scroll { offset: 10 }),
        &style(),
    );
    assert_eq!(past_end.segments()[0].bytes, 2..2);
}

#[test]
fn side_by_side_maps_each_side_and_pads_the_shorter_one() {
    let gutter = DiffRowGutter::new(2, DiffGutterShape::LineNumbersThenMarker);
    let layout = layout_diff_row(
        side_by_side("old text that wraps", "new"),
        31,
        DiffRowOptions::new(gutter),
        &style(),
    );
    let [old, new] = layout.columns() else {
        panic!("two columns");
    };
    assert_eq!(old.cells, 0..15);
    assert_eq!(new.cells, 16..31);
    assert!(layout.hit(0, 15).is_none(), "divider has no source");
    assert_eq!(symbol(&paint(&layout), 15, 0), "│");
    assert!(layout.height() > 1);
    for row in 0..layout.height() {
        assert_eq!(
            layout.hit(row, 20).map(|hit| hit.side),
            Some(DiffSourceSide::New)
        );
        assert_eq!(
            layout.hit(row, 3).map(|hit| hit.side),
            Some(DiffSourceSide::Old)
        );
    }
    let padded = layout.hit(layout.height() - 1, 28).unwrap();
    assert_eq!(padded.line, Some(8));
    assert_eq!(
        padded.byte_offset, 3,
        "padding rows map to the end of the line"
    );
    assert_eq!(layout.hit(0, 3).map(|hit| hit.line), Some(Some(7)));
}

#[test]
fn side_by_side_absent_side_is_blank_and_unmapped() {
    let layout = layout_diff_row(
        DiffRowItem::SideBySide {
            old: None,
            new: Some(DiffRowCell::new(
                DiffLineKind::Added,
                Some(3),
                DiffRowText::plain("added"),
            )),
        },
        40,
        DiffRowOptions::new(DiffRowGutter::new(
            2,
            DiffGutterShape::LineNumbersThenMarker,
        ))
        .with_columns(DiffSplitColumns::OldWidth(10)),
        &style(),
    );
    assert!(!layout.columns()[0].present);
    assert_eq!(layout.columns()[0].cells, 0..10);
    assert_eq!(layout.columns()[1].cells, 11..40);
    for column in 0..11 {
        assert!(layout.hit(0, column).is_none());
    }
    assert!(
        layout
            .segments()
            .iter()
            .all(|segment| segment.side == DiffSourceSide::New)
    );
}

#[test]
fn emphasis_and_syntax_style_exactly_their_graphemes() {
    let style = style();
    let keyword = Style::new().fg(Color::Magenta);
    let syntax = [DiffSyntaxRange::new(0, 3, keyword)];
    // "東京x": bytes 4..11.
    let changed = [ChangedRange::new(4, 11)];
    let text = "let 東京x = 1";
    let gutter = DiffRowGutter::new(1, DiffGutterShape::LineNumbersThenMarker);
    let layout = layout_diff_row(
        unified(
            DiffLineKind::Added,
            DiffRowText::plain(text)
                .with_syntax(&syntax)
                .with_changed_ranges(&changed),
        ),
        40,
        DiffRowOptions::new(gutter),
        &style,
    );
    let buffer = paint(&layout);
    let content = gutter.unified_width();
    let cell_style = |column: usize| {
        buffer
            .get(Point::new(u16::try_from(column).unwrap(), 0))
            .unwrap()
            .style
    };
    let base = style.added_row.patch(style.added);
    assert_eq!(cell_style(content), base.patch(keyword));
    assert_eq!(cell_style(content + 3), base, "space is unstyled");
    assert_eq!(cell_style(content + 4), base.patch(style.added_emphasis));
    assert_eq!(
        cell_style(content + 8),
        base.patch(style.added_emphasis),
        "x"
    );
    assert_eq!(cell_style(content + 9), base, "after the changed range");
    assert_eq!(
        cell_style(39),
        style.added_row,
        "padding uses the row style"
    );
}

#[test]
fn resizing_relays_out_deterministically() {
    let text = TEXTS[3];
    let options = options(
        DiffGutterShape::LineNumbersThenMarker,
        DiffRowOverflow::Wrap,
    );
    let at = |width| {
        layout_diff_row(
            unified(DiffLineKind::Added, DiffRowText::plain(text)),
            width,
            options,
            &style(),
        )
    };
    let wide = at(80);
    let narrow = at(20);
    assert_eq!(wide.height(), 1);
    assert!(narrow.height() > 2);
    assert_eq!(wide, at(80));
    let anchor = narrow.segments()[2].bytes.start;
    let row = wide
        .segments()
        .iter()
        .find(|segment| segment.bytes.contains(&anchor))
        .map(|segment| segment.row);
    assert_eq!(row, Some(0), "source offsets resolve to rows at any width");
}

#[test]
fn line_number_digits_are_computed_once() {
    assert_eq!(DiffRowGutter::line_number_digits(0), 1);
    assert_eq!(DiffRowGutter::line_number_digits(9), 1);
    assert_eq!(DiffRowGutter::line_number_digits(10), 2);
    assert_eq!(DiffRowGutter::line_number_digits(u32::MAX), 10);
}
