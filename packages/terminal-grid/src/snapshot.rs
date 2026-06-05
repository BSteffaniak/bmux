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
    pub revision: u64,
    #[serde(default)]
    pub content_revision: u64,
    pub width: u16,
    pub height: u16,
    pub mode: String,
    pub scrollback_rows: u32,
    pub cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_cursor: CursorSnapshot,
    #[serde(default)]
    pub saved_pending_wrap: bool,
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
}

impl GridSnapshot {
    #[must_use]
    pub fn from_grid(grid: &TerminalGrid, scrollback_offset: usize, rows: usize) -> Self {
        let requested_rows = if rows == usize::MAX {
            grid.height()
        } else {
            rows.max(grid.height())
        };
        let scrollback_rows = u32::try_from(grid.scrollback_rows_hint()).unwrap_or(u32::MAX);
        let projected_rows = grid.display_rows_unpadded(scrollback_offset, requested_rows);
        let selected_rows = projected_rows
            .into_iter()
            .map(|row| row_snapshot(&row, grid.width()))
            .collect();
        let cursor = cursor_snapshot(grid.cursor());
        Self {
            revision: grid.revision(),
            content_revision: grid.content_revision(),
            width: u16::try_from(grid.width()).unwrap_or(u16::MAX),
            height: u16::try_from(grid.height()).unwrap_or(u16::MAX),
            mode: match grid.mode() {
                GridMode::Main => "main".to_string(),
                GridMode::Alternate => "alternate".to_string(),
            },
            scrollback_rows,
            cursor,
            saved_cursor: cursor_snapshot(grid.saved_cursor()),
            saved_pending_wrap: grid.saved_pending_wrap(),
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
            styles: grid.palette().styles().to_vec(),
            rows: selected_rows,
        }
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

fn row_snapshot(row: &PhysicalRow, width: usize) -> RowSnapshot {
    let cells = row.visual_cells(width);
    let effective_len = cells
        .iter()
        .rposition(|cell| cell.text() != " " || cell.style() != StyleId::DEFAULT)
        .map_or(0, |index| index.saturating_add(1));
    let mut runs = Vec::new();
    let mut current_start = 0_usize;
    let mut current_style = None::<StyleId>;
    let mut current_text = String::new();

    for (index, cell) in cells.iter().take(effective_len).enumerate() {
        if cell.is_wide_continuation() {
            continue;
        }
        if cell.text() == " " && cell.style() == StyleId::DEFAULT && current_text.is_empty() {
            continue;
        }
        if current_style == Some(cell.style()) {
            current_text.push_str(cell.text());
            continue;
        }
        flush_run(&mut runs, current_start, current_style, &mut current_text);
        current_start = index;
        current_style = Some(cell.style());
        current_text.push_str(cell.text());
    }
    flush_run(&mut runs, current_start, current_style, &mut current_text);

    RowSnapshot {
        wrapped: row.wrapped(),
        runs,
    }
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
                b: 32
            })
        );
    }

    #[test]
    fn snapshot_omits_leading_default_spaces() {
        let mut grid = TerminalGrid::new(8, 1, GridLimits::default()).unwrap();
        grid.process(b"  bar");
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
