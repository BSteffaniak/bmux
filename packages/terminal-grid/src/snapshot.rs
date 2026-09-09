use crate::model::{Cursor, GridMode, PhysicalRow, ProtocolState, TerminalGrid};
use crate::style::{Style, StyleId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRunSnapshot {
    pub start_col: u16,
    pub text: String,
    pub style: StyleId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowSnapshot {
    pub wrapped: bool,
    pub runs: Vec<CellRunSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSnapshot {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollRegionSnapshot {
    pub top: u16,
    pub bottom: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSnapshot {
    /// Unknown in legacy snapshots; consumers must not infer complete history.
    #[serde(default)]
    pub history_truncated: Option<bool>,
    pub revision: u64,
    #[serde(default)]
    pub content_revision: u64,
    pub width: u16,
    pub height: u16,
    pub mode: String,
    pub scrollback_rows: u32,
    /// Cumulative scroll position, independent of retained history and screen mode.
    /// Absent in legacy payloads, which only supplied a retained-row count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_scrolled_rows: Option<u64>,
    pub cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_pending_wrap: bool,
    #[serde(default)]
    pub characters: crate::CharacterState,
    #[serde(default)]
    pub saved_characters: crate::CharacterState,
    #[serde(default)]
    pub current_style: Style,
    #[serde(default = "default_autowrap")]
    pub autowrap: bool,
    #[serde(default)]
    pub pending_wrap: bool,
    #[serde(default)]
    pub scroll_region: Option<ScrollRegionSnapshot>,
    #[serde(default)]
    pub protocol: ProtocolState,
    #[serde(default)]
    pub pending_bytes: Vec<u8>,
    pub styles: Vec<Style>,
    pub rows: Vec<RowSnapshot>,
    /// Bounded main-screen backing rows while the alternate screen is active.
    /// Viewport snapshots are also used to resume raw terminal output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_rows: Option<Vec<RowSnapshot>>,
}

impl GridSnapshot {
    /// # Panics
    /// Panics if snapshot storage cannot be allocated. Budgeted callers should
    /// use [`Self::try_from_grid`] instead.
    #[must_use]
    pub fn from_grid(grid: &TerminalGrid, scrollback_offset: usize, rows: usize) -> Self {
        Self::try_from_grid(grid, scrollback_offset, rows, usize::MAX)
            .expect("snapshot allocation failed")
    }

    /// Admit snapshot payload before projection. The budget includes conservative
    /// row/cell/run storage and palette costs, but excludes allocator overhead.
    #[must_use]
    pub fn try_from_grid(
        grid: &TerminalGrid,
        scrollback_offset: usize,
        rows: usize,
        budget: usize,
    ) -> Option<Self> {
        let requested_rows = if rows == usize::MAX {
            grid.height()
        } else {
            rows.max(grid.height())
        };
        // Selected row/cell/run storage is admitted at its allocation boundary.
        // Charging a full terminal rectangle here rejects sparse wide windows.
        let metadata = grid
            .palette()
            .styles()
            .len()
            .checked_mul(std::mem::size_of::<Style>())?
            .checked_add(std::mem::size_of::<Self>())?
            .checked_add(9)?;
        // Projection and wire runs can coexist, each holding the selected text.
        let text_budget = budget.checked_sub(metadata)? / 2;
        if !grid.snapshot_text_fits(scrollback_offset, requested_rows, text_budget) {
            return None;
        }
        let scrollback_rows = u32::try_from(grid.scrollback_rows_hint()).unwrap_or(u32::MAX);
        let mut remaining = budget.checked_sub(std::mem::size_of::<Self>())?;
        let projected_rows = grid.try_display_rows_charged(
            scrollback_offset,
            requested_rows,
            false,
            &mut remaining,
        )?;
        let selected_rows = try_snapshot_rows(&projected_rows, &mut remaining)?;
        let main_rows = if grid.mode() == GridMode::Alternate {
            let backing = grid.try_display_rows_charged(0, requested_rows, true, &mut remaining)?;
            let backing_bytes = physical_storage_bytes(&backing)?;
            let rows = try_snapshot_rows(&backing, &mut remaining)?;
            drop(backing);
            remaining = remaining.checked_add(backing_bytes)?;
            Some(rows)
        } else {
            None
        };
        let mut styles = Vec::new();
        reserve_snapshot_vec(&mut styles, grid.palette().styles().len(), &mut remaining)?;
        styles.extend_from_slice(grid.palette().styles());
        let mut mode = String::new();
        reserve_snapshot_text(&mut mode, 9, &mut remaining)?;
        mode.push_str(if grid.mode() == GridMode::Main {
            "main"
        } else {
            "alternate"
        });
        let cursor = cursor_snapshot(grid.cursor());
        Some(Self {
            revision: grid.revision(),
            content_revision: grid.content_revision(),
            width: u16::try_from(grid.width()).unwrap_or(u16::MAX),
            height: u16::try_from(grid.height()).unwrap_or(u16::MAX),
            mode,
            scrollback_rows,
            history_truncated: Some(
                grid.history_truncated() || grid.max_scrollback_offset() > scrollback_rows as usize,
            ),
            total_scrolled_rows: Some(grid.total_scrolled_rows()),
            cursor,
            saved_cursor: cursor_snapshot(grid.saved_cursor()),
            saved_pending_wrap: grid.saved_pending_wrap(),
            characters: grid.characters,
            saved_characters: grid.saved_characters,
            current_style: grid.current_style(),
            autowrap: grid.autowrap(),
            pending_wrap: grid.pending_wrap(),
            scroll_region: grid
                .scroll_region()
                .map(|(top, bottom)| ScrollRegionSnapshot {
                    top: u16::try_from(top).unwrap_or(u16::MAX),
                    bottom: u16::try_from(bottom).unwrap_or(u16::MAX),
                }),
            protocol: grid.protocol_state(),
            pending_bytes: Vec::new(),
            styles,
            rows: selected_rows,
            main_rows,
        })
    }
}

const fn default_autowrap() -> bool {
    true
}

fn cursor_snapshot(cursor: Cursor) -> CursorSnapshot {
    CursorSnapshot {
        row: u16::try_from(cursor.row).unwrap_or(u16::MAX),
        col: u16::try_from(cursor.col).unwrap_or(u16::MAX),
        visible: cursor.visible,
    }
}

fn physical_storage_bytes(rows: &Vec<PhysicalRow>) -> Option<usize> {
    rows.iter().try_fold(
        rows.capacity()
            .checked_mul(std::mem::size_of::<PhysicalRow>())?,
        |bytes, row| bytes.checked_add(row.allocated_bytes()?),
    )
}

pub(crate) fn reserve_snapshot_vec<T>(
    values: &mut Vec<T>,
    count: usize,
    remaining: &mut usize,
) -> Option<()> {
    let old = values.capacity();
    let required = values.len().checked_add(count)?;
    if required > old {
        // Old capacity is already charged. Admit the entire replacement while
        // both allocations may coexist, not merely their size difference.
        remaining.checked_sub(required.checked_mul(std::mem::size_of::<T>())?)?;
    }
    values.try_reserve_exact(count).ok()?;
    *remaining =
        remaining.checked_sub((values.capacity() - old).checked_mul(std::mem::size_of::<T>())?)?;
    Some(())
}

fn reserve_snapshot_text(text: &mut String, count: usize, remaining: &mut usize) -> Option<()> {
    let old = text.capacity();
    let required = text.len().checked_add(count)?;
    if required > old {
        remaining.checked_sub(required)?;
    }
    text.try_reserve_exact(count).ok()?;
    *remaining = remaining.checked_sub(text.capacity() - old)?;
    Some(())
}

fn try_snapshot_rows(rows: &[PhysicalRow], remaining: &mut usize) -> Option<Vec<RowSnapshot>> {
    let mut snapshots = Vec::new();
    reserve_snapshot_vec(&mut snapshots, rows.len(), remaining)?;
    for row in rows {
        snapshots.push(row_snapshot(row, remaining)?);
    }
    Some(snapshots)
}

fn row_snapshot(row: &PhysicalRow, remaining: &mut usize) -> Option<RowSnapshot> {
    // Stored rows omit implicit trailing blanks. Borrow their cells instead of
    // cloning every text allocation merely to pad with discardable blanks.
    let cells = row.cells();
    let effective_len = cells
        .iter()
        .rposition(|cell| !cell.is_discardable_blank())
        .map_or(0, |index| index.saturating_add(1));
    let mut runs = Vec::new();
    reserve_snapshot_vec(&mut runs, effective_len, remaining)?;
    let mut current_start = 0_usize;
    let mut current_style = None::<StyleId>;
    let mut current_text = String::new();

    for (index, cell) in cells.iter().take(effective_len).enumerate() {
        if cell.is_wide_continuation() {
            continue;
        }
        if cell.is_discardable_blank() && current_text.is_empty() {
            continue;
        }
        if current_style == Some(cell.style()) {
            reserve_snapshot_text(&mut current_text, cell.text().len(), remaining)?;
            current_text.push_str(cell.text());
            continue;
        }
        flush_run(&mut runs, current_start, current_style, &mut current_text);
        current_start = index;
        current_style = Some(cell.style());
        reserve_snapshot_text(&mut current_text, cell.text().len(), remaining)?;
        current_text.push_str(cell.text());
    }
    flush_run(&mut runs, current_start, current_style, &mut current_text);

    Some(RowSnapshot {
        wrapped: row.wrapped(),
        runs,
    })
}

fn flush_run(
    runs: &mut Vec<CellRunSnapshot>,
    start: usize,
    style: Option<StyleId>,
    text: &mut String,
) {
    if text.is_empty() {
        return;
    }
    runs.push(CellRunSnapshot {
        start_col: u16::try_from(start).unwrap_or(u16::MAX),
        text: std::mem::take(text),
        style: style.unwrap_or(StyleId::DEFAULT),
    });
}

#[cfg(test)]
mod tests {
    use crate::model::{GridLimits, TerminalGrid};

    #[test]
    fn fallible_projection_matches_selected_windows_and_rejects_before_projection() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 100,
            },
        )
        .unwrap();
        grid.process("ab界cdef\u{0301}ghijklmnop\r\nlast".as_bytes());
        for mode in [b"\x1b[?1049l".as_slice(), b"\x1b[?1049hwide"] {
            grid.process(mode);
            for offset in 0..8 {
                assert_eq!(
                    grid.try_display_rows(offset, 3, false).unwrap(),
                    grid.display_rows_unpadded(offset, 3)
                );
                assert_eq!(
                    grid.try_display_rows(offset, 3, true).unwrap(),
                    grid.main_display_rows(offset, 3)
                );
            }
        }
        crate::reflow::reset_projection_stats();
        let revision = grid.revision();
        assert!(super::GridSnapshot::try_from_grid(&grid, 0, 20, 0).is_none());
        assert_eq!(crate::reflow::projection_stats().physical_rows_projected, 0);
        assert_eq!(grid.revision(), revision);
    }

    #[test]
    fn sparse_wide_snapshot_admits_selected_storage() {
        let mut grid = TerminalGrid::new(1000, 2, GridLimits::default()).unwrap();
        grid.process(b"a\r\nb\r\nc\r\nd");
        for offset in 0..3 {
            assert_eq!(
                super::GridSnapshot::try_from_grid(&grid, offset, 2, 4096).unwrap(),
                grid.snapshot(offset, 2)
            );
        }
        grid.process(b"\x1b[?1049halt");
        assert_eq!(
            super::GridSnapshot::try_from_grid(&grid, 0, 2, 4096).unwrap(),
            grid.snapshot(0, 2)
        );
    }

    #[test]
    fn budgeted_snapshot_admits_metadata_before_projection() {
        let mut grid = TerminalGrid::new(80, 24, GridLimits::default()).unwrap();
        grid.process(b"hello");
        assert!(super::GridSnapshot::try_from_grid(&grid, 0, 24, 5).is_none());
        let snapshot = super::GridSnapshot::try_from_grid(&grid, 0, 100_000, 1_000_000).unwrap();
        assert_eq!(snapshot, grid.snapshot(0, 100_000));
        let stream = crate::TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        assert!(stream.try_snapshot(0, 24, 0).is_none());
        assert_eq!(
            stream.try_snapshot(0, 24, 1_000_000).unwrap(),
            stream.snapshot(0, 24)
        );
    }

    #[test]
    fn clone_and_snapshot_share_payload_admission() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.process(b"content");
        let mut budget = 100_000;
        let cloned = grid.try_clone_charged(&mut budget).unwrap();
        let clone_cost = 100_000 - budget;
        let snapshot_cost = (0..100_000)
            .find(|&limit| super::GridSnapshot::try_from_grid(&cloned, 0, 2, limit).is_some())
            .unwrap();
        let combined = clone_cost + snapshot_cost;
        let mut short = combined - 1;
        let copy = grid.try_clone_charged(&mut short).unwrap();
        assert!(super::GridSnapshot::try_from_grid(&copy, 0, 2, short).is_none());
        let mut exact = combined;
        let copy = grid.try_clone_charged(&mut exact).unwrap();
        assert_eq!(
            super::GridSnapshot::try_from_grid(&copy, 0, 2, exact).unwrap(),
            grid.snapshot(0, 2)
        );
        let mut rejected = clone_cost - 1;
        assert!(grid.try_clone_charged(&mut rejected).is_none());
        assert_eq!(rejected, clone_cost - 1);
    }

    #[test]
    fn wire_growth_admits_replacement_not_just_capacity_delta() {
        let mut text = String::from("abcd");
        let old = text.capacity();
        let count = old + 1;
        let mut remaining = text.len() + count - 1;
        assert!(super::reserve_snapshot_text(&mut text, count, &mut remaining).is_none());
        assert_eq!(text, "abcd");
        assert_eq!(text.capacity(), old);
        let mut values = vec![1_u64, 2];
        let old = values.capacity();
        let count = old + 1;
        let mut remaining = (values.len() + count) * std::mem::size_of::<u64>() - 1;
        assert!(super::reserve_snapshot_vec(&mut values, count, &mut remaining).is_none());
        assert_eq!(values, [1, 2]);
        assert_eq!(values.capacity(), old);
    }

    #[test]
    fn wire_reservations_reject_before_growth_and_charge_capacity() {
        let mut bytes = Vec::<u64>::new();
        let mut budget = 7;
        assert!(super::reserve_snapshot_vec(&mut bytes, 1, &mut budget).is_none());
        assert_eq!(bytes.capacity(), 0);
        assert_eq!(budget, 7);
        let mut text = String::new();
        assert!(super::reserve_snapshot_text(&mut text, 8, &mut budget).is_none());
        assert_eq!(text.capacity(), 0);
        assert_eq!(budget, 7);
        budget = 100;
        super::reserve_snapshot_text(&mut text, 10, &mut budget).unwrap();
        assert_eq!(budget, 100 - text.capacity());
        let before = budget;
        super::reserve_snapshot_text(&mut text, 1, &mut budget).unwrap();
        assert_eq!(budget, before);
    }

    #[test]
    fn budgeted_clone_preserves_both_screens_and_history() {
        let mut grid = TerminalGrid::new(
            4,
            2,
            GridLimits {
                scrollback_rows: 100,
            },
        )
        .unwrap();
        grid.process("a\u{0301}bcdefghijklmnop\x1b[?1049halt".as_bytes());
        let expected = grid.snapshot(0, 100);
        assert!(grid.try_clone_with_budget(0).is_none());
        assert!(
            grid.try_clone_with_budget(std::mem::size_of::<TerminalGrid>())
                .is_none()
        );
        let mut cloned = grid.try_clone_with_budget(100_000).unwrap();
        assert_eq!(cloned.snapshot(0, 100), expected);
        assert_eq!(grid.snapshot(0, 100), expected);
        for bytes in [b"\x1b[?1049l".as_slice(), b"\r\nmore"] {
            cloned.process(bytes);
            grid.process(bytes);
            assert_eq!(cloned.snapshot(0, 100), grid.snapshot(0, 100));
        }
    }

    #[test]
    fn snapshot_text_admission_selects_history_and_aggregates_cells() {
        let mut grid = TerminalGrid::new(
            4,
            2,
            GridLimits {
                scrollback_rows: 100,
            },
        )
        .unwrap();
        grid.process("a\u{0301}b\u{0301}c\u{0301}d\u{0301}efghijklmnop".as_bytes());
        assert!(!grid.snapshot_text_fits(2, 2, 10));
        assert!(grid.snapshot_text_fits(0, 2, 10));
        let revision = grid.revision();
        for offset in 0..4 {
            let snapshot = grid.snapshot(offset, 2);
            let bytes: usize = snapshot
                .rows
                .iter()
                .flat_map(|row| &row.runs)
                .map(|run| run.text.len())
                .sum();
            assert!(grid.snapshot_text_fits(offset, 2, bytes));
            if bytes > 0 {
                assert!(!grid.snapshot_text_fits(offset, 2, bytes - 1));
            }
        }
        assert_eq!(grid.revision(), revision);
        grid.process(b"\x1b[?1049halt");
        assert!(!grid.snapshot_text_fits(0, 2, 8));
        assert!(grid.snapshot_text_fits(0, 2, 11));
    }

    #[test]
    fn alternate_snapshot_bounds_backing_rows_independently_of_scrollback_offset() {
        let mut grid = TerminalGrid::new(
            20,
            3,
            GridLimits {
                scrollback_rows: 100,
            },
        )
        .unwrap();
        for _ in 0..50 {
            grid.process(b"history\r\n");
        }
        grid.process(b"main");
        let main = grid.snapshot(0, 3);
        grid.process(b"\x1b[?1049halt");
        for requested in [0, 1, 3, 7, usize::MAX] {
            let bound = if requested == usize::MAX {
                3
            } else {
                requested.max(3)
            };
            let snapshot = grid.snapshot(30, requested);
            assert!(snapshot.rows.len() <= 3);
            let backing = snapshot.main_rows.as_ref().unwrap();
            assert_eq!(backing.len(), bound);
            assert_eq!(&backing[backing.len() - 3..], main.rows.as_slice());
            let mut hydrated =
                TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
            hydrated.process(b"\x1b[?1049l");
            assert_eq!(hydrated.snapshot(0, 3).rows, main.rows);
        }
    }

    #[test]
    fn legacy_alternate_snapshot_decodes_without_inventing_main_content() {
        let mut grid = TerminalGrid::new(20, 3, GridLimits::default()).unwrap();
        grid.process(b"hidden main\x1b[?1049halt");
        let snapshot = grid.snapshot(0, 3);
        let mut wire = serde_json::to_value(&snapshot).unwrap();
        wire.as_object_mut().unwrap().remove("main_rows");
        let legacy: crate::GridSnapshot = serde_json::from_value(wire).unwrap();
        assert!(legacy.main_rows.is_none());
        let mut hydrated = TerminalGrid::from_snapshot(&legacy, GridLimits::default()).unwrap();
        assert_eq!(hydrated.snapshot(0, 3).rows, snapshot.rows);
        hydrated.process(b"\x1b[?1049l");
        // Old snapshots never transmitted the hidden screen; decoding cannot restore it.
        assert!(
            hydrated
                .snapshot(0, 3)
                .rows
                .iter()
                .all(|row| row.runs.is_empty())
        );
    }

    #[test]
    fn alternate_snapshot_wire_round_trip_preserves_hidden_screen() {
        let mut grid = TerminalGrid::new(20, 3, GridLimits::default()).unwrap();
        grid.process("\x1b[31mwide 界\r\nmain\x1b[?1049halt".as_bytes());
        let snapshot = grid.snapshot(0, 3);
        let decoded = serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(snapshot, decoded);
        let mut hydrated = TerminalGrid::from_snapshot(&decoded, GridLimits::default()).unwrap();
        grid.process(b"\x1b[?1049l!");
        hydrated.process(b"\x1b[?1049l!");
        assert_eq!(hydrated.snapshot(0, 3), grid.snapshot(0, 3));
    }

    #[test]
    fn snapshot_encodes_style_runs() {
        let mut grid = TerminalGrid::new(20, 2, GridLimits::default()).unwrap();
        grid.process(b"plain \x1b[31mred");
        let snapshot = grid.snapshot(0, 2);
        assert_eq!(snapshot.rows.len(), 2);
        assert!(snapshot.rows[0].runs.len() >= 2);
    }

    #[test]
    fn snapshot_round_trips_rows_styles_and_cursor() {
        let mut grid = TerminalGrid::new(8, 3, GridLimits::default()).unwrap();
        grid.process("plain \x1b[31mred\r\nwide 界".as_bytes());
        let snapshot = grid.snapshot(0, 20);

        let hydrated = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();

        assert_eq!(hydrated.width(), grid.width());
        assert_eq!(hydrated.height(), grid.height());
        assert_eq!(hydrated.cursor(), grid.cursor());
        assert_eq!(hydrated.palette().styles(), grid.palette().styles());
        assert_eq!(hydrated.viewport_rows(), grid.viewport_rows());
    }

    #[test]
    fn snapshot_preserves_current_style_for_future_output() {
        let mut grid = TerminalGrid::new(8, 2, GridLimits::default()).unwrap();
        grid.process(b"\x1b[31mred");
        let snapshot = grid.snapshot(0, 2);

        let mut hydrated = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        hydrated.process(b"X");

        let rows = hydrated.viewport_rows();
        let original_red = rows[0].cells()[0].style();
        let continued_red = rows[0].cells()[3].style();
        assert_eq!(original_red, continued_red);
        assert_eq!(
            hydrated.palette().get(continued_red).fg,
            Some(crate::Color::Indexed(1))
        );
    }

    #[test]
    fn snapshot_preserves_leading_styled_spaces() {
        let mut grid = TerminalGrid::new(8, 1, GridLimits::default()).unwrap();
        grid.process(b"\x1b[48;2;12;24;32m  bar");
        let snapshot = grid.snapshot(0, 1);

        assert_eq!(snapshot.rows[0].runs.len(), 1);
        assert_eq!(snapshot.rows[0].runs[0].start_col, 0);
        assert_eq!(snapshot.rows[0].runs[0].text, "  bar");

        let hydrated = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        let rows = hydrated.viewport_rows();
        let first = rows[0].cells()[0].style();
        let second = rows[0].cells()[1].style();
        let text = rows[0].cells()[2].style();
        assert_ne!(first, crate::style::StyleId::DEFAULT);
        assert_eq!(first, second);
        assert_eq!(first, text);
        assert_eq!(
            hydrated.palette().get(first).bg,
            Some(crate::Color::Rgb {
                r: 12,
                g: 24,
                b: 32,
            })
        );
    }

    #[test]
    fn snapshot_preserves_trailing_styled_spaces() {
        let mut grid = TerminalGrid::new(8, 1, GridLimits::default()).unwrap();
        grid.process(b"\x1b[48;2;12;24;32mbar   ");
        let snapshot = grid.snapshot(0, 1);

        assert_eq!(snapshot.rows[0].runs.len(), 1);
        assert_eq!(snapshot.rows[0].runs[0].start_col, 0);
        assert_eq!(snapshot.rows[0].runs[0].text, "bar   ");

        let hydrated = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        let rows = hydrated.viewport_rows();
        let text = rows[0].cells()[2].style();
        let trailing = rows[0].cells()[5].style();
        assert_ne!(trailing, crate::style::StyleId::DEFAULT);
        assert_eq!(text, trailing);
        assert_eq!(
            hydrated.palette().get(trailing).bg,
            Some(crate::Color::Rgb {
                r: 12,
                g: 24,
                b: 32,
            })
        );
    }

    #[test]
    fn snapshot_omits_discardable_default_spaces() {
        let mut grid = TerminalGrid::new(8, 1, GridLimits::default()).unwrap();
        grid.process(b"  bar   ");
        let snapshot = grid.snapshot(0, 1);

        assert_eq!(snapshot.rows[0].runs.len(), 1);
        assert_eq!(snapshot.rows[0].runs[0].start_col, 2);
        assert_eq!(snapshot.rows[0].runs[0].text, "bar");
    }

    #[test]
    fn snapshot_rejects_unknown_mode() {
        let mut snapshot = TerminalGrid::new(8, 2, GridLimits::default())
            .unwrap()
            .snapshot(0, 2);
        snapshot.mode = "unknown".to_string();

        assert!(TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).is_err());
    }
}
