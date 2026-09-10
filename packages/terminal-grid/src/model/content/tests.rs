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
fn viewport_reflow_excludes_scrolled_progress_and_retains_cursor_row() {
    let (stream, _) = capture(
        "progress 20%\r\ncompile one\r\ncompile two\r\nprogress 30%",
        40,
        3,
    );
    let before = stream.grid().snapshot(0, 20);
    let mut live = stream.grid().capture_viewport(10, budget()).unwrap();
    live.prepare(80, budget()).unwrap();
    assert_eq!(
        texts(&live.tail(28, budget().bytes).unwrap().rows),
        ["compile one", "compile two", "progress 30%"]
    );
    assert!(!live.viewport_prefix_continues());
    live.prepare(7, budget()).unwrap();
    let narrow = live.tail(28, budget().bytes).unwrap();
    assert_eq!(
        texts(&narrow.rows),
        ["compile", " one", "compile", " two", "progres", "s 30%"]
    );
    let anchor = narrow.anchors[1];
    live.prepare(80, budget()).unwrap();
    assert_eq!(live.resolve(anchor), Some(0));
    assert_eq!(stream.grid().snapshot(0, 20), before);

    let (stream, _) = capture("hello\r\n", 8, 3);
    let mut live = stream.grid().capture_viewport(11, budget()).unwrap();
    live.prepare(80, budget()).unwrap();
    assert_eq!(
        texts(&live.tail(28, budget().bytes).unwrap().rows),
        ["hello", ""]
    );
}

#[test]
fn viewport_soft_prefix_is_explicit_and_never_recaptured() {
    let (stream, _) = capture("abcdefghijkl", 4, 2);
    let mut live = stream.grid().capture_viewport(10, budget()).unwrap();
    assert!(live.viewport_prefix_continues());
    live.prepare(20, budget()).unwrap();
    let wide = live.tail(20, budget().bytes).unwrap();
    assert_eq!(texts(&wide.rows), ["efghijkl"]);
    assert!(wide.has_more_above);
    assert_eq!(
        wide.anchors[0],
        ContentAnchor {
            capture: 10,
            line: 0,
            column: 0
        }
    );
    live.prepare(3, budget()).unwrap();
    assert_eq!(
        texts(&live.tail(20, budget().bytes).unwrap().rows),
        ["efg", "hij", "kl"]
    );
}

#[test]
fn viewport_budget_is_independent_of_hidden_history() {
    let (stream, _) = capture(&"x".repeat(100_000), 8, 2);
    let allowance = ContentBudget {
        cells: 64,
        bytes: 4096,
    };
    let mut live = stream.grid().capture_viewport(1, allowance).unwrap();
    live.prepare(16, allowance).unwrap();
    assert_eq!(live.row_count(), Some(1));
    assert!(matches!(
        stream
            .grid()
            .capture_viewport(2, ContentBudget { cells: 0, bytes: 0 }),
        Err(HistorySliceError::BudgetExhausted)
    ));
    let (alternate, _) = capture("\x1b[?1049h", 8, 2);
    assert!(matches!(
        alternate.grid().capture_viewport(3, budget()),
        Err(HistorySliceError::Unavailable)
    ));
}

#[test]
fn source_ranges_preserve_wide_columns_empty_lines_and_continuations() {
    let (_, mut projection) = capture("界z\r\n\r\nlast", 8, 4);
    projection.prepare(1, budget()).unwrap();
    let window = projection.tail(30, budget().bytes).unwrap();
    assert_eq!(window.sources.len(), window.rows.len());
    assert_eq!(window.sources[0].start.column, 0);
    assert_eq!(window.sources[0].end.column, 2);
    assert!(window.sources[0].continues);
    assert_eq!(window.sources[1].start.column, 2);
    assert_eq!(window.sources[1].end.column, 3);
    assert!(!window.sources[1].continues);
    let empty = window.sources[2];
    assert_eq!(empty.start, empty.end);
    assert!(!empty.continues);
    assert_eq!(projection.resolve(empty.start), Some(2));
    for (anchor, source) in window.anchors.iter().zip(&window.sources) {
        assert_eq!(*anchor, source.start);
    }
    let selected = projection.window(0..1, budget().bytes).unwrap();
    assert!(selected.sources[0].continues);
    assert!(selected.has_more_below);
}

#[test]
fn screen_source_ranges_mark_both_clipped_edges_and_charge_metadata() {
    let (stream, _) = capture("\x1b[?1049h界a界", 8, 2);
    let before = stream.grid().snapshot(0, 2);
    let crop = stream
        .grid()
        .screen_window_with_sources(1..4, 0..1, budget())
        .unwrap();
    assert_eq!(
        crop.sources,
        vec![ScreenRowSource {
            row: 0,
            columns: 1..4,
            clipped_left: true,
            clipped_right: true
        }]
    );
    assert_eq!(texts(&crop.rows), [" a"]);
    assert_eq!(crop.revision, stream.grid().content_revision());
    let full = stream
        .grid()
        .screen_window_with_sources(0..8, 0..1, budget())
        .unwrap();
    assert!(!full.sources[0].clipped_left);
    assert!(!full.sources[0].clipped_right);
    assert_eq!(texts(&full.rows), ["界a界"]);
    assert_eq!(stream.grid().snapshot(0, 2), before);
    assert!(matches!(
        stream.grid().screen_window_with_sources(
            0..8,
            0..1,
            ContentBudget {
                cells: 100,
                bytes: 0
            }
        ),
        Err(HistorySliceError::BudgetExhausted)
    ));
    let empty = stream
        .grid()
        .screen_window_with_sources(99..100, 0..1, budget())
        .unwrap();
    assert_eq!(empty.sources[0].columns, 8..8);
    assert!(!empty.sources[0].clipped_left);
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
