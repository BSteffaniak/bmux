use super::*;
use crate::{GridLimits, TerminalGridStream, row_text};

fn budget() -> ContentBudget {
    ContentBudget {
        cells: 2_000_000,
        bytes: 128 * 1024 * 1024,
    }
}
fn capture(text: &str, width: u16, height: u16) -> (TerminalGridStream, ContentProjection) {
    let mut stream = TerminalGridStream::new(
        width,
        height,
        GridLimits {
            scrollback_rows: 100_000,
        },
    )
    .unwrap();
    stream.process(text.as_bytes());
    let projection = stream
        .grid()
        .capture_content(42, 100_000, budget())
        .unwrap();
    (stream, projection)
}
fn texts(rows: &[PhysicalRow]) -> Vec<String> {
    rows.iter()
        .map(|row| row_text(row, row.cells().len()))
        .collect()
}

#[test]
fn projection_work_report_for_long_history_and_single_line() {
    use std::time::Instant;
    for (name, text) in [
        ("history", "line\r\n".repeat(20_000)),
        ("single-line", "x".repeat(100_000)),
    ] {
        let (stream, _) = capture(&text, 80, 3);
        let start = Instant::now();
        let mut projection = stream.grid().capture_content(9, 100, budget()).unwrap();
        let capture_time = start.elapsed();
        let start = Instant::now();
        projection.prepare(40, budget()).unwrap();
        let index_time = start.elapsed();
        let start = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(projection.tail(28, budget().bytes).unwrap());
        }
        eprintln!(
            "{name}: capture={capture_time:?}, index={index_time:?}, 100 windows={:?}",
            start.elapsed()
        );
    }
}

#[test]
fn truncation_empty_windows_and_wide_anchors_are_explicit() {
    let mut stream = TerminalGridStream::new(4, 2, GridLimits { scrollback_rows: 2 }).unwrap();
    stream.process(b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
    let mut projection = stream.grid().capture_content(1, 1, budget()).unwrap();
    projection.prepare(4, budget()).unwrap();
    let window = projection.tail(1, budget().bytes).unwrap();
    assert!(window.history_truncated);
    assert!(window.has_more_above);
    assert!(projection.tail(0, 0).unwrap().rows.is_empty());
    let (_, mut wide) = capture("界z", 8, 2);
    wide.prepare(8, budget()).unwrap();
    assert_eq!(
        wide.resolve(ContentAnchor {
            capture: 42,
            line: 0,
            column: 1
        }),
        None
    );
    assert_eq!(
        wide.resolve(ContentAnchor {
            capture: 42,
            line: 0,
            column: 2
        }),
        Some(0)
    );
}

#[test]
fn widths_are_independent_and_anchors_follow_logical_columns() {
    let (mut stream, mut projection) = capture("abcdefghij\r\nsecond", 4, 2);
    let before = stream.grid().snapshot(0, 20);
    projection.prepare(3, budget()).unwrap();
    let narrow = projection.tail(30, budget().bytes).unwrap();
    assert_eq!(
        texts(&narrow.rows),
        ["abc", "def", "ghi", "j", "sec", "ond"]
    );
    let anchor = narrow.anchors[2];
    projection.prepare(10, budget()).unwrap();
    assert_eq!(projection.resolve(anchor), Some(0));
    assert_eq!(
        texts(&projection.tail(30, budget().bytes).unwrap().rows),
        ["abcdefghij", "second"]
    );
    projection.prepare(3, budget()).unwrap();
    assert_eq!(
        projection.tail(30, budget().bytes).unwrap().rows,
        narrow.rows
    );
    assert_eq!(stream.grid().snapshot(0, 20), before);
    stream.process(b"!");
    let mut next = stream.grid().capture_content(43, 30, budget()).unwrap();
    next.prepare(3, budget()).unwrap();
    assert_eq!(next.resolve(anchor), None);
}

#[test]
fn indexed_projection_matches_grid_reflow_for_unicode_and_styles() {
    for text in [
        "a界bcd界e",
        "e\u{301}\tq\r\nnext",
        "\x1b[41mabc   \x1b[0m\r\nz",
        "old\rnew\x1b[K",
        "a\r\n\r\nb",
    ] {
        let (stream, mut projection) = capture(text, 8, 3);
        for width in [1, 2, 3, 7, 15] {
            projection.prepare(width, budget()).unwrap();
            let mut reference = stream.grid().clone();
            reference.resize(u16::try_from(width).unwrap(), 30).unwrap();
            assert_eq!(
                projection.tail(100, budget().bytes).unwrap().rows,
                reference.main_content_rows(),
                "{text:?} width {width}"
            );
        }
    }
}

#[test]
fn budgets_fail_before_unbounded_line_capture_and_preserve_index() {
    let (_, mut projection) = capture(&"x".repeat(10_000), 80, 2);
    projection.prepare(80, budget()).unwrap();
    let original = projection.row_count();
    assert_eq!(
        projection.prepare(1, ContentBudget { cells: 0, bytes: 0 }),
        Err(HistorySliceError::BudgetExhausted)
    );
    assert_eq!(projection.row_count(), original);
    assert!(matches!(
        projection.tail(2, 0),
        Err(HistorySliceError::BudgetExhausted)
    ));
    let (stream, _) = capture(&"x".repeat(10_000), 80, 2);
    assert!(matches!(
        stream.grid().capture_content(
            1,
            1,
            ContentBudget {
                cells: 100,
                bytes: 1000
            }
        ),
        Err(HistorySliceError::BudgetExhausted)
    ));
}

#[test]
fn tail_window_does_not_revisit_a_giant_line() {
    let (_, mut projection) = capture(&"x".repeat(100_000), 80, 2);
    projection.prepare(40, budget()).unwrap();
    crate::reflow::reset_projection_stats();
    let tail = projection.tail(2, 32_000).unwrap();
    assert_eq!(tail.rows.len(), 2);
    assert!(tail.has_more_above);
    assert_eq!(crate::reflow::projection_stats().physical_rows_projected, 2);
    // A cache hit needs no scanning or index allocation allowance.
    projection
        .prepare(40, ContentBudget { cells: 0, bytes: 0 })
        .unwrap();
}

#[test]
fn bounded_tail_matches_existing_full_content_without_materializing_history() {
    let (stream, _) = capture(&"line\r\n".repeat(10_000), 80, 3);
    let expected = stream.grid().main_content_rows();
    crate::reflow::reset_projection_stats();
    assert_eq!(
        stream.grid().main_content_tail_rows(2),
        expected[expected.len() - 2..]
    );
    assert!(crate::reflow::projection_stats().physical_rows_projected <= 4);
    assert!(stream.grid().main_content_tail_rows(0).is_empty());
}

#[test]
fn positioned_crop_is_reversible_and_does_not_break_wide_cells() {
    let (stream, _) = capture("main\x1b[?1049h\x1b[Habcdef界", 10, 3);
    let grid = stream.grid();
    let before = grid.snapshot(0, 3);
    assert_eq!(
        texts(&grid.screen_window(2..5, 0..1, budget()).unwrap()),
        ["cde"]
    );
    assert_eq!(
        texts(&grid.screen_window(7..9, 0..1, budget()).unwrap()),
        [""]
    );
    assert_eq!(
        texts(&grid.screen_window(0..10, 0..1, budget()).unwrap()),
        ["abcdef界"]
    );
    assert_eq!(grid.snapshot(0, 3), before);
    let mut main = grid.capture_content(2, 10, budget()).unwrap();
    main.prepare(10, budget()).unwrap();
    assert_eq!(
        texts(&main.tail(10, budget().bytes).unwrap().rows),
        ["main"]
    );
}
