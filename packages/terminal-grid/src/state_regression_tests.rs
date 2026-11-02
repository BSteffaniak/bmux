use crate::{GridDeltaBatch, GridLimits, GridSnapshot, TerminalGrid, TerminalGridStream};

fn stream(width: u16, height: u16) -> TerminalGridStream {
    TerminalGridStream::new(width, height, GridLimits::default()).unwrap()
}

fn hydrate(snapshot: &GridSnapshot) -> TerminalGridStream {
    TerminalGridStream::from_snapshot(snapshot, GridLimits::default()).unwrap()
}

fn row(grid: &TerminalGridStream, index: usize) -> String {
    crate::row_text(&grid.grid().viewport_rows()[index], grid.grid().width())
}

fn assert_wide_integrity(grid: &TerminalGrid) {
    for row in grid.viewport_rows() {
        for (col, cell) in row.cells().iter().enumerate() {
            if cell.is_wide_continuation() {
                assert!(col > 0);
                assert_eq!(row.cells()[col - 1].width(), 2);
                assert!(!row.cells()[col - 1].is_wide_continuation());
            } else if cell.width() == 2 {
                assert!(col + 1 < grid.width());
                assert!(row.cells()[col + 1].is_wide_continuation());
            }
        }
    }
}

#[test]
fn dec_saved_state_restores_style_origin_autowrap_after_hydration() {
    let mut grid = stream(12, 6);
    grid.process(b"\x1b[2;5r\x1b[?6h\x1b[?7l\x1b[31m\x1b[2;4H\x1b7");
    let saved = grid.snapshot(0, 6);
    let mut grid = hydrate(&saved);
    grid.process(b"\x1b[?6l\x1b[?7h\x1b[32m\x1b[6;1H\x1b8X");
    let after = grid.snapshot(0, 6);
    assert!(after.terminal_state.origin_mode);
    assert!(!after.autowrap);
    assert_eq!(after.current_style, saved.current_style);
    assert_eq!((after.cursor.row, after.cursor.col), (2, 4));
    assert_eq!(row(&grid, 2).trim_end(), "   X");
}

#[test]
fn origin_cup_relative_moves_and_axis_moves_apply_margin_once() {
    let mut grid = stream(12, 7);
    grid.process(b"\x1b[3;6r\x1b[?6h");
    assert_eq!(grid.grid().cursor().row, 2);
    grid.process(b"\x1b[2;4H\x1b[B\x1b[7G");
    assert_eq!((grid.grid().cursor().row, grid.grid().cursor().col), (4, 6));
    grid.process(b"\x1b[99B");
    assert_eq!(grid.grid().cursor().row, 5);
    grid.process(b"\x1b[99A");
    assert_eq!(grid.grid().cursor().row, 2);
    grid.process(b"\x1b[2d");
    assert_eq!(grid.grid().cursor().row, 3);
    grid.process(b"\x1b[?6l");
    assert_eq!(grid.grid().cursor().row, 0);
}

#[test]
fn insert_mode_custom_and_cleared_tabs_survive_snapshots_and_deltas() {
    let mut grid = stream(20, 3);
    grid.process(b"abcdef\x1b[1;3H\x1b[4h\x1b[3g\x1b[1;6H\x1bH\x1b[1;3H");
    let before = grid.snapshot(0, 3);
    let mut replica = before.clone();
    let mut resumed = hydrate(&before);
    grid.process(b"XY\tZ");
    resumed.process(b"XY\tZ");
    let after = grid.snapshot(0, 3);
    GridDeltaBatch::between(&before, &after)
        .unwrap()
        .apply_to_snapshot(&mut replica)
        .unwrap();
    assert_eq!(replica, after);
    assert_eq!(resumed.snapshot(0, 3), after);
    assert_eq!(row(&grid, 0).trim_end(), "abXYcZdef");
    grid.process(b"\x1b[3g\r\t");
    assert_eq!(grid.grid().cursor().col, 19);
    let mut resumed = hydrate(&grid.snapshot(0, 3));
    resumed.process(b"\r\t");
    assert_eq!(resumed.grid().cursor().col, 19);
    resumed.grid_mut().reset();
    resumed.process(b"\t");
    assert_eq!(resumed.grid().cursor().col, 8);
    assert!(!resumed.snapshot(0, 3).terminal_state.insert_mode);
}

#[test]
fn mode_47_retains_hidden_alternate_across_hydration_and_delta() {
    let mut grid = stream(12, 3);
    grid.process(b"main\x1b[?47h\x1b[Hretained\x1b[?47l");
    let before = grid.snapshot(0, 3);
    let mut resumed = hydrate(&before);
    resumed.process(b"\x1b[?47h");
    assert_eq!(row(&resumed, 0).trim_end(), "retained");
    resumed.process(b"\x1b[?47l\x1b[?1047h");
    assert_eq!(row(&resumed, 0).trim_end(), "");
    resumed.process(b"new\x1b[?1047l\x1b[?47h");
    assert_eq!(row(&resumed, 0).trim_end(), "");
    grid.process(b"\x1b[?47h\x1b[Hupdated\x1b[?47l");
    let after = grid.snapshot(0, 3);
    let mut replica = before.clone();
    GridDeltaBatch::between(&before, &after)
        .unwrap()
        .apply_to_snapshot(&mut replica)
        .unwrap();
    let mut resumed = hydrate(&replica);
    resumed.process(b"\x1b[?47h");
    assert_eq!(row(&resumed, 0).trim_end(), "updatedd");
}

#[test]
fn ed3_clears_only_history_and_preserves_viewport_cursor_and_style() {
    let mut grid = stream(12, 2);
    grid.process(b"old\r\nvisible\r\n\x1b[31mbottom");
    let before = grid.snapshot(0, 2);
    let display_revision = grid.grid().display_erase_revision();
    grid.process(b"\x1b[3J");
    let after = grid.snapshot(0, 2);
    assert_eq!(after.rows, before.rows);
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(after.current_style, before.current_style);
    assert_eq!(after.scrollback_rows, 0);
    assert_eq!(grid.grid().display_erase_revision(), display_revision);
    assert!(grid.grid().history_erase_revision() > 0);
}

#[test]
fn overlapping_wide_writes_erases_inserts_deletes_and_clipping_are_valid() {
    for operation in ["X", "界", "\x1b[X", "\x1b[K", "\x1b[1K", "\x1b[@", "\x1b[P"] {
        for col in 1..=6 {
            let mut grid = stream(6, 2);
            grid.process("a界界b".as_bytes());
            grid.process(format!("\x1b[1;{col}H{operation}").as_bytes());
            assert_wide_integrity(grid.grid());
            let resumed = hydrate(&grid.snapshot(0, 2));
            assert_wide_integrity(resumed.grid());
        }
    }
    let mut grid = stream(4, 2);
    grid.process("\x1b[?7l\x1b[1;4H界".as_bytes());
    assert_wide_integrity(grid.grid());
    grid.process("\x1b[1;2H界".as_bytes());
    grid.resize(2, 2).unwrap();
    assert_wide_integrity(grid.grid());
}

#[test]
fn state_hydration_rejects_unknown_or_corrupt_representation_and_defaults_legacy() {
    let grid = stream(12, 3);
    let original = grid.snapshot(0, 3);
    let mut bad = original.clone();
    bad.terminal_state.version = 2;
    assert!(TerminalGrid::from_snapshot(&bad, GridLimits::default()).is_err());
    for stops in [vec![4, 4], vec![9, 4], vec![12]] {
        bad = original.clone();
        bad.terminal_state.tab_stops = Some(stops);
        assert!(TerminalGrid::from_snapshot(&bad, GridLimits::default()).is_err());
    }
    let mut value = serde_json::to_value(&original).unwrap();
    value.as_object_mut().unwrap().remove("terminal_state");
    value.as_object_mut().unwrap().remove("retained_alt_rows");
    let legacy: GridSnapshot = serde_json::from_value(value).unwrap();
    let mut resumed = hydrate(&legacy);
    resumed.process(b"\t");
    assert_eq!(resumed.grid().cursor().col, 8);
    assert!(!legacy.terminal_state.origin_mode);
    let mut target = original.clone();
    bad = original.clone();
    bad.revision += 1;
    bad.terminal_state.version = 8;
    let delta = GridDeltaBatch::between(&original, &bad).unwrap();
    assert!(delta.apply_to_snapshot(&mut target).is_err());
    assert_eq!(target, original);
}

#[test]
fn state_resize_clips_tabs_and_wide_cells_without_recreating_cleared_stops() {
    let mut grid = stream(12, 3);
    grid.process("界界界\x1b[3g\x1b[1;11H\x1bH".as_bytes());
    grid.resize(1, 3).unwrap();
    assert_wide_integrity(grid.grid());
    let snapshot = grid.snapshot(0, 3);
    assert_eq!(snapshot.terminal_state.tab_stops, Some(Vec::new()));
    let mut resumed = hydrate(&snapshot);
    resumed.resize(20, 4).unwrap();
    resumed.process(b"\r\t");
    assert_eq!(resumed.grid().cursor().col, 19);
}

#[test]
fn retained_alternate_corruption_rejects_hydration_and_delta_atomically() {
    let mut grid = stream(6, 2);
    grid.process(b"\x1b[?47hX\x1b[?47l");
    let base = grid.snapshot(0, 2);
    for kind in 0..4 {
        let mut corrupt = base.clone();
        corrupt.revision += 1;
        let rows = corrupt.retained_alt_rows.as_mut().unwrap();
        match kind {
            0 => {
                rows.pop();
            }
            1 => rows[0].runs[0].style = crate::StyleId(999),
            2 => rows[0].runs[0].start_col = 6,
            _ => {
                rows[0].runs[0].start_col = 5;
                rows[0].runs[0].text = "界".to_owned();
            }
        }
        assert!(TerminalGrid::from_snapshot(&corrupt, GridLimits::default()).is_err());
        let delta = GridDeltaBatch::between(&base, &corrupt).unwrap();
        let mut replica = base.clone();
        assert!(delta.apply_to_snapshot(&mut replica).is_err());
        assert_eq!(replica, base);
    }
}

#[test]
fn mode_1049_restores_saved_wrap_style_and_cursor_after_hydration() {
    let mut grid = stream(4, 3);
    grid.process(b"\x1b[31mabcd\x1b[?1049h\x1b[32mALT");
    let mut grid = hydrate(&grid.snapshot(0, 3));
    grid.process(b"\x1b[?1049lX");
    assert_eq!(row(&grid, 0), "abcd");
    assert_eq!(row(&grid, 1).trim_end(), "X");
    assert_eq!((grid.grid().cursor().row, grid.grid().cursor().col), (1, 1));
    assert_eq!(
        grid.snapshot(0, 3).current_style.fg,
        Some(crate::Color::Indexed(1))
    );
}

#[test]
fn state_sequences_are_chunk_boundary_invariant_including_hydrated_pending_bytes() {
    let bytes = "\x1b[2;4r\x1b[?6h\x1b[31m\x1b7\x1b[?7l\x1b[4h\x1b[3g\x1b[4G\x1bH界\x1b8\tZ\x1b[?47h\x1b[Halt\x1b[?47l".as_bytes();
    let mut expected = stream(12, 5);
    expected.process(bytes);
    let expected = expected.snapshot(0, 5);
    for split in 0..=bytes.len() {
        let mut grid = stream(12, 5);
        grid.process(&bytes[..split]);
        let mut resumed = hydrate(&grid.snapshot(0, 5));
        resumed.process(&bytes[split..]);
        let mut actual = resumed.snapshot(0, 5);
        // Pending decoder transitions carry their own revision when a chunk ends
        // mid-sequence; compare terminal semantics, not chunk-count metadata.
        actual.revision = expected.revision;
        assert_eq!(actual, expected, "split {split}");
    }
}
