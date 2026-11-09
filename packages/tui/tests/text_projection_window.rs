//! Viewport-bounded text projection and source-offset lookup over retained
//! `TextBlock` layout.

use bmux_tui::component::Component;
use bmux_tui::prelude::*;

const WIDTHS: [u64; 7] = [0, 1, 2, 3, 5, 9, 40];

fn mixed_text() -> Text {
    Text::from_lines(vec![
        Line::from_spans(vec![
            Span::styled("bold ", Style::new().fg(Color::Green)),
            Span::raw("plain words that wrap"),
        ]),
        Line::raw("日本語のテキストを折り返す"),
        Line::raw(""),
        Line::raw("family 👩‍👩‍👧 emoji 🎉🎉 end"),
        Line::from_spans(vec![
            Span::raw("e\u{301}cole "),
            Span::styled("n\u{303}o", Style::new().fg(Color::Red)),
            Span::raw(" combining"),
        ]),
    ])
}

fn layout_at(block: &TextBlock, width: u64) -> LayoutNode {
    block.layout(Constraints::for_width(width), &mut LayoutCx::new())
}

#[test]
fn window_equals_slice_of_full_projection_for_every_width() {
    for wrap in [TextWrap::Word, TextWrap::Character, TextWrap::None] {
        let block = TextBlock::new(mixed_text()).wrap(wrap);
        for width in WIDTHS {
            let layout = layout_at(&block, width);
            let full = block.projection(&layout);
            let total = u64::try_from(full.len()).unwrap();
            assert_eq!(total, layout.size.height, "{wrap:?} width {width}");
            for start in 0..=total + 1 {
                for end in start..=total + 2 {
                    let window = block.projection_window(&layout, start..end);
                    let first = usize::try_from(start.min(total)).unwrap();
                    let last = usize::try_from(end.min(total)).unwrap();
                    assert_eq!(
                        window.rows(),
                        &full[first..last],
                        "{wrap:?} {width} {start}..{end}"
                    );
                    assert_eq!(window.first_row(), start.min(total));
                    assert_eq!(window.total_rows(), total);
                    assert_eq!(window.row_range(), start.min(total)..end.min(total));
                }
            }
        }
    }
}

#[test]
fn reversed_or_empty_ranges_project_nothing() {
    let block = TextBlock::new(mixed_text());
    let layout = layout_at(&block, 6);
    #[allow(clippy::reversed_empty_ranges)]
    let reversed = block.projection_window(&layout, 4..2);
    assert!(reversed.rows().is_empty());
    assert!(block.projection_window(&layout, 3..3).rows().is_empty());
}

#[test]
fn deep_window_of_huge_block_materializes_only_requested_rows() {
    let lines = (0..50_000).map(|index| Line::raw(format!("row {index}")));
    let block = TextBlock::new(Text::from_lines(lines.collect::<Vec<_>>()));
    let layout = layout_at(&block, 20);
    let window = block.projection_window(&layout, 49_990..60_000);
    assert_eq!(window.total_rows(), 50_000);
    assert_eq!(window.row_range(), 49_990..50_000);
    let texts = window
        .indexed_rows()
        .map(|(row, projected)| (row, projected.line.plain_text()))
        .collect::<Vec<_>>();
    assert_eq!(texts.len(), 10);
    assert_eq!(texts[0], (49_990, "row 49990".to_owned()));
    assert_eq!(texts[9], (49_999, "row 49999".to_owned()));
    assert_eq!(
        block.row_for_source_offset(&layout, window.rows()[3].source_range.start),
        Some(49_993)
    );
}

#[test]
fn every_rendered_offset_maps_back_to_its_row() {
    for wrap in [TextWrap::Word, TextWrap::Character, TextWrap::None] {
        let block = TextBlock::new(mixed_text()).wrap(wrap).trim(true);
        for width in WIDTHS {
            let layout = layout_at(&block, width);
            let rows = block.projection(&layout);
            for (index, row) in rows.iter().enumerate() {
                let index = u64::try_from(index).unwrap();
                assert_eq!(
                    block.source_range_for_row(&layout, index),
                    Some(row.source_range.clone())
                );
                let first_with_start = rows
                    .iter()
                    .position(|other| other.source_range.start == row.source_range.start)
                    .map(|position| u64::try_from(position).unwrap());
                for offset in row.source_range.clone() {
                    let expected = if offset == row.source_range.start {
                        first_with_start
                    } else {
                        Some(index)
                    };
                    assert_eq!(
                        block.row_for_source_offset(&layout, offset),
                        expected,
                        "{wrap:?} width {width} offset {offset}"
                    );
                }
            }
            let height = layout.size.height;
            assert_eq!(block.source_range_for_row(&layout, height), None);
        }
    }
}

#[test]
fn unrendered_offsets_map_to_the_preceding_row_and_past_end_is_unmapped() {
    let text = "alpha beta\ngamma";
    let block = TextBlock::new(text);
    let layout = layout_at(&block, 6);
    let rows = block
        .projection(&layout)
        .into_iter()
        .map(|row| (row.line.plain_text(), row.source_range))
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        vec![
            ("alpha ".to_owned(), 0..6),
            ("beta".to_owned(), 6..10),
            ("gamma".to_owned(), 11..16),
        ]
    );
    // The newline separator at byte 10 is not rendered.
    assert_eq!(block.row_for_source_offset(&layout, 10), Some(1));
    assert_eq!(block.row_for_source_offset(&layout, text.len()), Some(2));
    assert_eq!(block.row_for_source_offset(&layout, text.len() + 1), None);
}

#[test]
fn source_anchor_survives_reflow_across_widths_and_resize_back() {
    let block = TextBlock::new(mixed_text());
    let wide = layout_at(&block, 40);
    let anchor = block
        .source_range_for_row(&wide, 3)
        .expect("row 3 is measured")
        .start;
    for width in [1, 3, 7, 12, 40] {
        let layout = layout_at(&block, width);
        let row = block
            .row_for_source_offset(&layout, anchor)
            .expect("anchor offset is mapped");
        let range = block.source_range_for_row(&layout, row).unwrap();
        assert!(
            range.start <= anchor && (anchor < range.end || range.is_empty()),
            "width {width}: row {row} {range:?} does not hold {anchor}"
        );
    }
    let back = layout_at(&block, 40);
    assert_eq!(block.row_for_source_offset(&back, anchor), Some(3));
}

#[test]
fn source_rows_satisfy_the_source_mapped_contract_with_offsets() {
    let block = TextBlock::new(mixed_text());
    for width in WIDTHS {
        let layout = layout_at(&block, width);
        let rows = block.source_rows(&layout);
        assert_eq!(rows.len(), layout.size.height);
        let shifted = OffsetSourceRows::new(rows, 5);
        for row in 0..rows.len() {
            let (position, start) = rows.source_for_row(row).unwrap();
            assert!(start <= row);
            assert_eq!(rows.row_for_source(&position), Some(start));
            assert_eq!(shifted.source_for_row(row + 5), Some((position, start + 5)));
            assert_eq!(shifted.row_for_source(&position), Some(start + 5));
        }
        assert_eq!(shifted.source_for_row(4), None);
        assert_eq!(rows.source_for_row(rows.len()), None);
    }
}

#[test]
fn layout_without_text_geometry_projects_and_maps_nothing() {
    let block = TextBlock::new("text");
    let foreign = LayoutNode::leaf("other".into(), LogicalSize::new(4, 1));
    let window = block.projection_window(&foreign, 0..10);
    assert!(window.rows().is_empty());
    assert_eq!(window.total_rows(), 0);
    assert_eq!(block.row_for_source_offset(&foreign, 0), None);
    assert!(block.source_rows(&foreign).is_empty());
}

#[test]
fn deep_paint_matches_the_projection_window() {
    let lines = (0..20_000).map(|index| Line::raw(format!("line {index} 日本")));
    let block = TextBlock::new(Text::from_lines(lines.collect::<Vec<_>>()));
    let layout = layout_at(&block, 12);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
    let mut frame = Frame::new(&mut buffer);
    PaintCx::new(&mut frame).with_child(0, -15_000, LocalRect::new(0, 15_000, 12, 3), |cx| {
        block.paint(&layout, cx);
    });
    let window = block.projection_window(&layout, 15_000..15_003);
    for (index, row) in window.rows().iter().enumerate() {
        let y = u16::try_from(index).unwrap();
        let painted = buffer.row_symbols(y).unwrap();
        assert_eq!(
            painted.trim_end(),
            row.line.plain_text().trim_end(),
            "row {index}"
        );
    }
}
