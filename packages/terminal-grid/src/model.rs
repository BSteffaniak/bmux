use crate::reflow::{project_logical_line, projected_logical_line_row_count};
mod content;
use crate::snapshot::{GridSnapshot, RowSnapshot};
use crate::style::{Color, Style, StyleId, StylePalette};
pub use content::{ContentAnchor, ContentBudget, ContentProjection, ContentRows};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use thiserror::Error;
use unicode_width::UnicodeWidthChar;

/// Limits for retained terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridLimits {
    /// Retained rows above the live viewport.
    pub scrollback_rows: usize,
}

impl Default for GridLimits {
    fn default() -> Self {
        Self {
            scrollback_rows: 10_000,
        }
    }
}

/// Why a bounded logical-history slice stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySliceEnd {
    /// More cells remain in this captured logical line.
    Continue,
    /// The captured logical line ends with a hard break.
    HardBreak,
    /// The pending logical line continues into the live screen.
    Open,
}

/// Borrowed history content; offsets count terminal columns, not UTF-8 bytes.
/// Line indices are valid only within the same grid capture and revision.
#[derive(Debug)]
pub struct HistorySlice<'a> {
    pub cells: &'a [Cell],
    pub next_cell_offset: usize,
    pub end: HistorySliceEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySliceError {
    StaleRevision,
    Unavailable,
    InvalidOffset,
    BudgetExhausted,
}

/// Consumer-owned assembly of one captured logical line. The opaque identity
/// belongs to the caller; terminal algorithms do not interpret it.
#[derive(Debug)]
pub struct HistoryLineAssembly {
    identity: u128,
    line_index: usize,
    next_offset: usize,
    cells: Vec<Cell>,
    remaining_bytes: usize,
    retain_trailing_cells: bool,
    prefix_unavailable: bool,
    end: HistorySliceEnd,
}

impl HistoryLineAssembly {
    #[must_use]
    pub const fn new(
        identity: u128,
        line_index: usize,
        max_bytes: usize,
        prefix_unavailable: bool,
    ) -> Self {
        Self {
            identity,
            line_index,
            next_offset: 0,
            cells: Vec::new(),
            remaining_bytes: max_bytes,
            retain_trailing_cells: false,
            prefix_unavailable,
            end: HistorySliceEnd::Continue,
        }
    }

    /// Preserve all admitted cells, including trailing blanks, in projection and
    /// anchor mapping. This does not allocate or extend the admitted content.
    pub const fn retain_trailing_cells(&mut self) {
        self.retain_trailing_cells = true;
    }

    /// Admit and copy an ordered slice. Failure leaves the assembled content and
    /// continuation unchanged; allocation is fallible and text plus cell metadata
    /// is charged before copying. Styles must already use the consumer's palette.
    ///
    /// # Errors
    /// Rejects another capture/line, gaps, overlaps, malformed column counts,
    /// appends after termination, and exhausted allocation/content budgets.
    pub fn append(
        &mut self,
        identity: u128,
        line_index: usize,
        offset: usize,
        slice: &HistorySlice<'_>,
    ) -> Result<(), HistorySliceError> {
        if identity != self.identity {
            return Err(HistorySliceError::StaleRevision);
        }
        if line_index != self.line_index
            || offset != self.next_offset
            || self.end != HistorySliceEnd::Continue
        {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut next = offset;
        for cell in slice.cells {
            if !(1..=2).contains(&cell.width()) {
                return Err(HistorySliceError::InvalidOffset);
            }
            next = next
                .checked_add(usize::from(cell.width()))
                .ok_or(HistorySliceError::InvalidOffset)?;
        }
        if next != slice.next_cell_offset
            || (next == offset && slice.end == HistorySliceEnd::Continue)
        {
            return Err(HistorySliceError::InvalidOffset);
        }
        let charged = history_cells_bytes(slice.cells);
        let remaining = self
            .remaining_bytes
            .checked_sub(charged)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let copied = try_clone_cells(slice.cells).ok_or(HistorySliceError::BudgetExhausted)?;
        self.cells
            .try_reserve_exact(copied.len())
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        self.cells.extend(copied);
        self.next_offset = next;
        self.remaining_bytes = remaining;
        self.end = slice.end;
        Ok(())
    }

    /// Project a bounded physical-row range after complete assembly.
    ///
    /// # Errors
    /// Returns unavailable for unfinished content and budget exhaustion when
    /// selected row allocation exceeds the caller's projection allowance.
    pub fn project(
        &self,
        width: usize,
        range: std::ops::Range<usize>,
        budget: usize,
    ) -> Result<Vec<PhysicalRow>, HistorySliceError> {
        if width == 0 || self.completed().is_none() {
            return Err(HistorySliceError::Unavailable);
        }
        let count = crate::reflow::projected_logical_line_row_count_retained(
            &self.cells,
            width,
            self.retain_trailing_cells,
        );
        let range = range.start.min(count)..range.end.min(count);
        let metadata = (range.end.saturating_sub(range.start))
            .checked_mul(std::mem::size_of::<PhysicalRow>())
            .and_then(|bytes| {
                bytes.checked_add(crate::reflow::projected_cell_storage_retained(
                    &self.cells,
                    width,
                    range.clone(),
                    self.retain_trailing_cells,
                )?)
            })
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let mut remaining = budget
            .checked_sub(metadata)
            .ok_or(HistorySliceError::BudgetExhausted)?;
        crate::reflow::admit_logical_text_retained(
            &self.cells,
            width,
            range.clone(),
            &mut remaining,
            self.retain_trailing_cells,
        )
        .ok_or(HistorySliceError::BudgetExhausted)?;
        let rows = crate::reflow::try_project_logical_line_window_retained(
            &self.cells,
            width,
            range,
            self.retain_trailing_cells,
        )
        .ok_or(HistorySliceError::BudgetExhausted)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(rows.len())
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        for mut row in rows {
            if self.end == HistorySliceEnd::Open {
                row.set_wrapped(true);
            }
            output.push(row);
        }
        Ok(output)
    }

    /// Map a presentation row boundary to a stable logical-column anchor.
    #[must_use]
    pub fn column_for_row(&self, width: usize, row: usize) -> Option<usize> {
        self.completed()?;
        crate::reflow::logical_column_for_row_retained(
            &self.cells,
            width,
            row,
            self.retain_trailing_cells,
        )
    }

    /// Map a stable cell-boundary anchor to its row at another width.
    #[must_use]
    pub fn row_for_column(&self, width: usize, column: usize) -> Option<usize> {
        self.completed()?;
        crate::reflow::row_for_logical_column_retained(
            &self.cells,
            width,
            column,
            self.retain_trailing_cells,
        )
    }

    #[must_use]
    pub fn projected_rows(&self, width: usize) -> usize {
        crate::reflow::projected_logical_line_row_count_retained(
            &self.cells,
            width.max(1),
            self.retain_trailing_cells,
        )
    }

    #[must_use]
    pub const fn next_offset(&self) -> usize {
        self.next_offset
    }

    #[must_use]
    pub const fn prefix_unavailable(&self) -> bool {
        self.prefix_unavailable
    }

    /// Only a terminated line is available for presentation. An Open ending
    /// still continues into the live screen and must not become a hard break.
    #[must_use]
    pub fn completed(&self) -> Option<(&[Cell], HistorySliceEnd)> {
        (self.end != HistorySliceEnd::Continue).then_some((&self.cells, self.end))
    }
}

/// Terminal grid mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    Main,
    Alternate,
}

/// Bounded row window requested from a terminal grid projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridRowWindow {
    pub scrollback_offset: usize,
    pub rows: usize,
}

/// Rows projected for a bounded display window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedRows {
    pub rows: Vec<PhysicalRow>,
    pub has_more_above: bool,
}

/// Grid cursor in viewport-relative coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolMode {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "protocol modes mirror independent DEC/private terminal toggles for stable snapshot encoding"
)]
pub struct ProtocolState {
    #[serde(default)]
    pub mouse_x10: bool,
    #[serde(default)]
    pub mouse_press_release: bool,
    #[serde(default)]
    pub mouse_button_motion: bool,
    #[serde(default)]
    pub mouse_any_motion: bool,
    #[serde(default)]
    pub mouse_utf8: bool,
    #[serde(default)]
    pub mouse_sgr: bool,
    #[serde(default)]
    pub mouse_urxvt: bool,
    #[serde(default)]
    pub application_cursor: bool,
    #[serde(default)]
    pub application_keypad: bool,
    #[serde(default)]
    pub bracketed_paste: bool,
}

impl ProtocolState {
    #[must_use]
    pub const fn mouse_mode(self) -> MouseProtocolMode {
        if self.mouse_any_motion {
            MouseProtocolMode::AnyMotion
        } else if self.mouse_button_motion {
            MouseProtocolMode::ButtonMotion
        } else if self.mouse_press_release {
            MouseProtocolMode::PressRelease
        } else if self.mouse_x10 {
            MouseProtocolMode::Press
        } else {
            MouseProtocolMode::None
        }
    }

    #[must_use]
    pub const fn mouse_encoding(self) -> MouseProtocolEncoding {
        if self.mouse_sgr || self.mouse_urxvt {
            MouseProtocolEncoding::Sgr
        } else if self.mouse_utf8 {
            MouseProtocolEncoding::Utf8
        } else {
            MouseProtocolEncoding::Default
        }
    }
}

/// One display cell. Wide-character spacer cells are represented with
/// `wide_continuation = true` and empty text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    text: String,
    style: StyleId,
    width: u8,
    wide_continuation: bool,
}

impl Cell {
    #[must_use]
    pub fn new(text: impl Into<String>, style: StyleId, width: u8) -> Self {
        Self {
            text: text.into(),
            style,
            width: width.max(1),
            wide_continuation: false,
        }
    }

    #[must_use]
    pub fn spacer(style: StyleId) -> Self {
        Self {
            text: String::new(),
            style,
            width: 0,
            wide_continuation: true,
        }
    }

    #[must_use]
    pub fn blank(style: StyleId) -> Self {
        Self::new(" ", style, 1)
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn style(&self) -> StyleId {
        self.style
    }

    #[must_use]
    pub const fn width(&self) -> u8 {
        self.width
    }

    #[must_use]
    pub const fn is_wide_continuation(&self) -> bool {
        self.wide_continuation
    }

    #[must_use]
    pub fn is_discardable_blank(&self) -> bool {
        self.text.len() == 1
            && self.text.as_bytes()[0] == b' '
            && self.style == StyleId::DEFAULT
            && !self.wide_continuation
    }

    pub(crate) fn append_combining(&mut self, ch: char) {
        self.text.push(ch);
    }
}

/// One physical terminal row. `wrapped` means this row soft-wraps into the next
/// row and therefore belongs to the same logical line during resize reflow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhysicalRow {
    cells: Vec<Cell>,
    wrapped: bool,
}

impl PhysicalRow {
    pub(crate) fn allocated_bytes(&self) -> Option<usize> {
        self.cells.iter().try_fold(
            self.cells
                .capacity()
                .checked_mul(std::mem::size_of::<Cell>())?,
            |bytes, cell| bytes.checked_add(cell.text.capacity()),
        )
    }

    pub(crate) fn try_set_projected_cell(
        &mut self,
        col: usize,
        cell: &Cell,
        width: usize,
    ) -> Option<()> {
        let end = col.checked_add(if cell.width == 2 && col + 1 < width {
            2
        } else {
            1
        })?;
        self.cells
            .try_reserve_exact(end.saturating_sub(self.cells.len()))
            .ok()?;
        while self.cells.len() < end {
            // Reserve blank text explicitly; projection can expose gaps after
            // discardable cells have been trimmed.
            let mut text = String::new();
            text.try_reserve_exact(1).ok()?;
            text.push(' ');
            self.cells.push(Cell::new(text, StyleId::DEFAULT, 1));
        }
        let mut text = String::new();
        text.try_reserve_exact(cell.text.len()).ok()?;
        text.push_str(&cell.text);
        self.cells[col] = Cell {
            text,
            style: cell.style,
            width: cell.width,
            wide_continuation: cell.wide_continuation,
        };
        if end > col + 1 {
            self.cells[col + 1] = Cell::spacer(cell.style);
        }
        self.trim_trailing_blanks();
        Some(())
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a row whose columns are pre-filled with `style` blanks.
    ///
    /// A default-styled fill produces an empty row, matching the compact
    /// representation where unwritten columns are implicit.
    pub(crate) fn erase_filled(width: usize, style: StyleId) -> Self {
        if style == StyleId::DEFAULT {
            return Self::new();
        }
        Self {
            cells: vec![Cell::blank(style); width],
            wrapped: false,
        }
    }

    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    #[must_use]
    pub const fn wrapped(&self) -> bool {
        self.wrapped
    }

    pub fn set_wrapped(&mut self, wrapped: bool) {
        self.wrapped = wrapped;
    }

    pub(crate) fn set_cell(&mut self, col: usize, cell: Cell) {
        if self.cells.len() <= col {
            self.cells
                .resize_with(col + 1, || Cell::blank(StyleId::DEFAULT));
        }
        self.cells[col] = cell;
        self.trim_trailing_blanks();
    }

    pub(crate) fn cell_mut(&mut self, col: usize) -> Option<&mut Cell> {
        self.cells.get_mut(col)
    }

    pub(crate) fn clear_range(&mut self, start: usize, end: usize, style: StyleId) {
        if start >= end {
            return;
        }
        if style == StyleId::DEFAULT {
            if start >= self.cells.len() {
                return;
            }
            let clamped_end = end.min(self.cells.len());
            for cell in &mut self.cells[start..clamped_end] {
                *cell = Cell::blank(StyleId::DEFAULT);
            }
        } else {
            // A styled erase fill is materialized even past the current row
            // extent, because background color erase must colorize columns that
            // have never been written.
            if self.cells.len() < end {
                self.cells
                    .resize_with(end, || Cell::blank(StyleId::DEFAULT));
            }
            for cell in &mut self.cells[start..end] {
                *cell = Cell::blank(style);
            }
        }
        self.trim_trailing_blanks();
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.cells.truncate(len);
        self.trim_trailing_blanks();
    }

    fn trim_trailing_blanks(&mut self) {
        while self.cells.last().is_some_and(Cell::is_discardable_blank) {
            self.cells.pop();
        }
    }

    pub(crate) fn visual_cells(&self, width: usize) -> Vec<Cell> {
        let mut cells = self.cells.clone();
        cells.resize_with(width, || Cell::blank(StyleId::DEFAULT));
        cells
    }
}

#[derive(Debug, Error)]
pub enum TerminalGridError {
    #[error("terminal dimensions must be non-zero")]
    ZeroDimensions,
    #[error("invalid terminal grid snapshot: {0}")]
    InvalidSnapshot(&'static str),
}

/// Structured terminal state with bounded main-screen scrollback and isolated
/// alternate-screen viewport.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "history provenance is independent of terminal protocol flags, not a mutually exclusive mode"
)]
pub struct TerminalGrid {
    width: usize,
    height: usize,
    limits: GridLimits,
    main_history: VecDeque<LogicalLine>,
    history_bytes: usize,
    history_truncated: bool,
    main_history_projected_rows: OnceLock<usize>,
    pending_history_cells: Vec<Cell>,
    main_rows: VecDeque<PhysicalRow>,
    alt_rows: Vec<PhysicalRow>,
    mode: GridMode,
    cursor: Cursor,
    saved_cursor: Cursor,
    saved_pending_wrap: bool,
    pub(crate) characters: crate::CharacterState,
    pub(crate) saved_characters: crate::CharacterState,
    current_style: Style,
    palette: StylePalette,
    revision: u64,
    content_revision: u64,
    total_scrolled_rows: u64,
    autowrap: bool,
    pending_wrap: bool,
    scroll_region: Option<(usize, usize)>,
    protocol: ProtocolState,
}

#[derive(Debug, Default)]
struct LogicalLine {
    cells: Vec<Cell>,
    cached_projection: Mutex<Option<(usize, usize)>>,
}

impl Clone for LogicalLine {
    fn clone(&self) -> Self {
        Self {
            cells: self.cells.clone(),
            cached_projection: Mutex::new(
                *self
                    .cached_projection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
        }
    }
}

impl LogicalLine {
    fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells,
            cached_projection: Mutex::new(None),
        }
    }

    fn projected_row_count(&self, width: usize) -> usize {
        let cached = *self
            .cached_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_width, rows)) = cached
            && cached_width == width
        {
            return rows;
        }
        // Cells are immutable, so concurrent misses may compute independently.
        // Only publishing the width/count pair needs the cache lock.
        let rows = projected_logical_line_row_count(&self.cells, width);
        *self
            .cached_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((width, rows));
        rows
    }
}

#[derive(Debug, Clone, Copy)]
struct CursorAnchor {
    logical_line: usize,
    logical_col: usize,
}

impl TerminalGrid {
    /// Create a new grid.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn new(width: u16, height: u16, limits: GridLimits) -> Result<Self, TerminalGridError> {
        let width = usize::from(width);
        let height = usize::from(height);
        if width == 0 || height == 0 {
            return Err(TerminalGridError::ZeroDimensions);
        }
        let mut main_rows = VecDeque::new();
        for _ in 0..height {
            main_rows.push_back(PhysicalRow::new());
        }
        Ok(Self {
            width,
            height,
            limits,
            main_history: VecDeque::new(),
            history_bytes: 0,
            history_truncated: false,
            main_history_projected_rows: OnceLock::from(0),
            pending_history_cells: Vec::new(),
            main_rows,
            alt_rows: vec![PhysicalRow::new(); height],
            mode: GridMode::Main,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
            saved_cursor: Cursor::default(),
            saved_pending_wrap: false,
            characters: crate::CharacterState::default(),
            saved_characters: crate::CharacterState::default(),
            current_style: Style::default(),
            palette: StylePalette::default(),
            revision: 0,
            content_revision: 0,
            total_scrolled_rows: 0,
            autowrap: true,
            pending_wrap: false,
            scroll_region: None,
            protocol: ProtocolState::default(),
        })
    }

    /// Hydrate a grid from a structured snapshot.
    ///
    /// The snapshot may contain either a full retained history or a bounded
    /// slice. Hydration preserves every encoded row, then pads with blank rows
    /// when the slice is shorter than the viewport height.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot has zero dimensions or an unknown
    /// screen mode.
    pub fn from_snapshot(
        snapshot: &GridSnapshot,
        limits: GridLimits,
    ) -> Result<Self, TerminalGridError> {
        let width = usize::from(snapshot.width);
        let height = usize::from(snapshot.height);
        if width == 0 || height == 0 {
            return Err(TerminalGridError::ZeroDimensions);
        }
        let mode = match snapshot.mode.as_str() {
            "main" => GridMode::Main,
            "alternate" => GridMode::Alternate,
            _ => return Err(TerminalGridError::InvalidSnapshot("unknown screen mode")),
        };
        let palette = StylePalette::from_styles(snapshot.styles.clone());
        let main_snapshot_rows = match mode {
            GridMode::Main => snapshot.rows.as_slice(),
            GridMode::Alternate => snapshot.main_rows.as_deref().unwrap_or_default(),
        };
        let rows = main_snapshot_rows
            .iter()
            .map(|row| row_from_snapshot(row, width))
            .collect::<Vec<_>>();
        let mut main_history = VecDeque::new();
        let mut pending_history_cells = Vec::new();
        let mut main_rows = VecDeque::new();
        let viewport_start = rows.len().saturating_sub(height);
        hydrate_logical_history(
            &rows[..viewport_start],
            width,
            &mut main_history,
            &mut pending_history_cells,
        );
        main_rows.extend(rows.into_iter().skip(viewport_start));
        while main_rows.len() < height {
            main_rows.push_front(PhysicalRow::new());
        }
        let mut alt_rows = vec![PhysicalRow::new(); height];
        if mode == GridMode::Alternate {
            for (target, row) in alt_rows.iter_mut().zip(&snapshot.rows) {
                *target = row_from_snapshot(row, width);
            }
        }
        let main_history_projected_rows: usize = main_history
            .iter()
            .map(|line| line.projected_row_count(width))
            .sum();
        let mut grid = Self {
            width,
            height,
            limits,
            history_bytes: main_history
                .iter()
                .map(|line| history_cells_bytes(&line.cells))
                .sum::<usize>()
                .saturating_add(history_cells_bytes(&pending_history_cells)),
            history_truncated: snapshot.history_truncated.unwrap_or(true),
            main_history,
            main_history_projected_rows: OnceLock::from(main_history_projected_rows),
            pending_history_cells,
            main_rows,
            alt_rows,
            mode,
            cursor: Cursor {
                row: usize::from(snapshot.cursor.row),
                col: usize::from(snapshot.cursor.col),
                visible: snapshot.cursor.visible,
            },
            saved_cursor: Cursor::default(),
            saved_pending_wrap: snapshot.saved_pending_wrap,
            characters: snapshot.characters,
            saved_characters: snapshot.saved_characters,
            current_style: snapshot.current_style,
            palette,
            revision: snapshot.revision,
            content_revision: snapshot.content_revision,
            total_scrolled_rows: snapshot
                .total_scrolled_rows
                .unwrap_or(u64::from(snapshot.scrollback_rows)),
            autowrap: snapshot.autowrap,
            pending_wrap: snapshot.pending_wrap,
            scroll_region: snapshot.scroll_region.map(|region| {
                let top = usize::from(region.top).min(height.saturating_sub(1));
                let bottom = usize::from(region.bottom).min(height.saturating_sub(1));
                if top < bottom {
                    (top, bottom)
                } else {
                    (0, height.saturating_sub(1))
                }
            }),
            protocol: snapshot.protocol,
        };
        grid.saved_cursor = Cursor {
            row: usize::from(snapshot.saved_cursor.row),
            col: usize::from(snapshot.saved_cursor.col),
            visible: snapshot.saved_cursor.visible,
        };
        grid.clamp_cursor();
        grid.evict_excess_history();
        Ok(grid)
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    #[must_use]
    pub const fn total_scrolled_rows(&self) -> u64 {
        self.total_scrolled_rows
    }

    #[must_use]
    pub const fn mode(&self) -> GridMode {
        self.mode
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    #[must_use]
    pub const fn protocol_state(&self) -> ProtocolState {
        self.protocol
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn content_revision(&self) -> u64 {
        self.content_revision
    }

    #[must_use]
    pub fn palette(&self) -> &StylePalette {
        &self.palette
    }

    /// Process a self-contained byte slice with a fresh parser.
    ///
    /// Use [`TerminalGridStream`](crate::TerminalGridStream) for live PTY
    /// streams where escape sequences can be split across chunks.
    pub fn process(&mut self, bytes: &[u8]) {
        crate::parser::process(self, bytes);
    }

    /// Resize the terminal. Main-screen finalized scrollback remains logical
    /// and width-independent; only the live viewport tail is projected to the
    /// new width.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), TerminalGridError> {
        let width = usize::from(width);
        let height = usize::from(height);
        if width == 0 || height == 0 {
            return Err(TerminalGridError::ZeroDimensions);
        }
        if self.width == width && self.height == height {
            return Ok(());
        }
        let main_cursor = match self.mode {
            GridMode::Main => self.cursor,
            GridMode::Alternate => self.saved_cursor,
        };
        let resized_main_cursor = self.resize_main_viewport(width, height, main_cursor);
        match self.mode {
            GridMode::Main => self.cursor = resized_main_cursor,
            GridMode::Alternate => {
                self.saved_cursor = resized_main_cursor;
                self.saved_pending_wrap = false;
            }
        }
        if self.width != width {
            self.main_history_projected_rows.take();
        }
        self.width = width;
        self.height = height;
        self.pending_wrap = false;
        self.evict_excess_history();
        self.alt_rows.resize_with(height, PhysicalRow::new);
        self.scroll_region = self.scroll_region.and_then(|(top, bottom)| {
            let clamped_top = top.min(height.saturating_sub(1));
            let clamped_bottom = bottom.min(height.saturating_sub(1));
            (clamped_top < clamped_bottom).then_some((clamped_top, clamped_bottom))
        });
        for row in &mut self.alt_rows {
            row.truncate(width);
            row.set_wrapped(false);
        }
        self.cursor = clamp_cursor_to_dimensions(self.cursor, width, height);
        self.saved_cursor = clamp_cursor_to_dimensions(self.saved_cursor, width, height);
        self.bump_content_revision();
        Ok(())
    }

    pub fn set_mode(&mut self, mode: GridMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.pending_wrap = false;
        self.cursor = Cursor {
            row: 0,
            col: 0,
            visible: self.cursor.visible,
        };
        if mode == GridMode::Alternate {
            self.alt_rows = vec![PhysicalRow::new(); self.height];
        }
        self.bump_content_revision();
    }

    pub(crate) fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor.visible = visible;
        self.bump_revision();
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
        self.saved_characters = self.characters;
        self.saved_pending_wrap = self.pending_wrap;
        self.bump_revision();
    }

    pub(crate) fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.characters = self.saved_characters;
        self.pending_wrap = self.saved_pending_wrap;
        self.clamp_cursor();
        self.bump_revision();
    }

    pub(crate) fn set_autowrap(&mut self, enabled: bool) {
        self.autowrap = enabled;
        self.bump_revision();
    }

    pub(crate) fn set_mouse_tracking_mode(&mut self, mode: MouseProtocolMode, enabled: bool) {
        let before = self.protocol;
        match mode {
            MouseProtocolMode::None => {}
            MouseProtocolMode::Press => self.protocol.mouse_x10 = enabled,
            MouseProtocolMode::PressRelease => self.protocol.mouse_press_release = enabled,
            MouseProtocolMode::ButtonMotion => self.protocol.mouse_button_motion = enabled,
            MouseProtocolMode::AnyMotion => self.protocol.mouse_any_motion = enabled,
        }
        if self.protocol != before {
            self.bump_revision();
        }
    }

    pub(crate) fn set_mouse_encoding(&mut self, encoding: MouseProtocolEncoding, enabled: bool) {
        let before = self.protocol;
        match encoding {
            MouseProtocolEncoding::Default => {}
            MouseProtocolEncoding::Utf8 => self.protocol.mouse_utf8 = enabled,
            MouseProtocolEncoding::Sgr => self.protocol.mouse_sgr = enabled,
        }
        if self.protocol != before {
            self.bump_revision();
        }
    }

    pub(crate) fn set_mouse_urxvt_encoding(&mut self, enabled: bool) {
        let before = self.protocol;
        self.protocol.mouse_urxvt = enabled;
        if self.protocol != before {
            self.bump_revision();
        }
    }

    pub(crate) fn set_application_cursor(&mut self, enabled: bool) {
        if self.protocol.application_cursor != enabled {
            self.protocol.application_cursor = enabled;
            self.bump_revision();
        }
    }

    pub(crate) fn set_application_keypad(&mut self, enabled: bool) {
        if self.protocol.application_keypad != enabled {
            self.protocol.application_keypad = enabled;
            self.bump_revision();
        }
    }

    pub(crate) fn set_bracketed_paste(&mut self, enabled: bool) {
        if self.protocol.bracketed_paste != enabled {
            self.protocol.bracketed_paste = enabled;
            self.bump_revision();
        }
    }

    pub(crate) fn set_scroll_region(&mut self, top: Option<usize>, bottom: Option<usize>) {
        let top = top.unwrap_or(0).min(self.height.saturating_sub(1));
        let bottom = bottom.unwrap_or_else(|| self.height.saturating_sub(1));
        let bottom = bottom.min(self.height.saturating_sub(1));
        self.scroll_region = (top < bottom).then_some((top, bottom));
        self.move_cursor_to(0, 0);
    }

    pub(crate) fn move_cursor_to(&mut self, row: usize, col: usize) {
        self.pending_wrap = false;
        self.cursor.row = row.min(self.height.saturating_sub(1));
        self.cursor.col = col.min(self.width.saturating_sub(1));
        self.bump_revision();
    }

    pub(crate) fn move_cursor_relative(&mut self, rows: isize, cols: isize) {
        let row = self.cursor.row.saturating_add_signed(rows);
        let col = self.cursor.col.saturating_add_signed(cols);
        self.move_cursor_to(row, col);
    }

    pub(crate) fn carriage_return(&mut self) {
        self.pending_wrap = false;
        self.cursor.col = 0;
        self.bump_revision();
    }

    pub(crate) fn backspace(&mut self) {
        self.pending_wrap = false;
        self.cursor.col = self.cursor.col.saturating_sub(1);
        self.bump_revision();
    }

    pub(crate) fn tab(&mut self) {
        self.pending_wrap = false;
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.width.saturating_sub(1));
        self.bump_revision();
    }

    pub(crate) fn linefeed(&mut self) {
        self.pending_wrap = false;
        let mut content_changed = false;
        let (top, bottom) = self.effective_scroll_region();
        if self.cursor.row == bottom {
            if self.mode == GridMode::Main && top == 0 && bottom + 1 == self.height {
                self.scroll_up_one();
            } else {
                self.scroll_region_up(top, bottom, 1);
            }
            content_changed = true;
        } else if self.cursor.row < self.height.saturating_sub(1) {
            self.cursor.row += 1;
        }
        if content_changed {
            self.bump_content_revision();
        } else {
            self.bump_revision();
        }
    }

    pub(crate) fn reverse_index(&mut self) {
        self.pending_wrap = false;
        let mut content_changed = false;
        let (top, bottom) = self.effective_scroll_region();
        if self.cursor.row == top {
            self.scroll_region_down(top, bottom, 1);
            content_changed = true;
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        if content_changed {
            self.bump_content_revision();
        } else {
            self.bump_revision();
        }
    }

    pub(crate) fn print_char(&mut self, ch: char) {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width == 0 {
            self.append_combining(ch);
            self.bump_content_revision();
            return;
        }
        let char_width = char_width.min(2);
        if self.autowrap && self.pending_wrap {
            self.pending_wrap = false;
            self.mark_current_row_wrapped();
            self.cursor.col = 0;
            self.linefeed();
        }
        if self.autowrap && self.cursor.col + char_width > self.width {
            self.mark_current_row_wrapped();
            self.cursor.col = 0;
            self.linefeed();
        }

        let style = self.palette.intern(self.current_style);
        let row = self.cursor_absolute_row();
        let col = self.cursor.col;
        let cell = Cell::new(ch.to_string(), style, u8::try_from(char_width).unwrap_or(1));
        self.active_row_mut(row).set_cell(col, cell);
        if char_width == 2 && col + 1 < self.width {
            self.active_row_mut(row)
                .set_cell(col + 1, Cell::spacer(style));
        }

        if col + char_width >= self.width {
            self.cursor.col = self.width.saturating_sub(1);
            self.pending_wrap = self.autowrap;
        } else {
            self.cursor.col = col + char_width;
            self.pending_wrap = false;
        }
        self.bump_content_revision();
    }

    pub(crate) fn erase_display(&mut self, mode: usize) {
        let fill = self.erase_style();
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.cursor.row + 1..self.height {
                    self.fill_viewport_row(row, fill);
                }
            }
            1 => {
                for row in 0..self.cursor.row {
                    self.fill_viewport_row(row, fill);
                }
                self.erase_line(1);
            }
            2 | 3 => {
                for row in 0..self.height {
                    self.fill_viewport_row(row, fill);
                }
                if mode == 3 && self.mode == GridMode::Main {
                    self.history_truncated |= self.history_line_count() > 0;
                    self.history_bytes = 0;
                    self.main_history.clear();
                    self.main_history_projected_rows = OnceLock::from(0);
                    self.pending_history_cells.clear();
                }
            }
            _ => {}
        }
        self.bump_content_revision();
    }

    pub(crate) fn erase_line(&mut self, mode: usize) {
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        let fill = self.erase_style();
        match mode {
            0 => self.active_row_mut(row).clear_range(col, width, fill),
            1 => self
                .active_row_mut(row)
                .clear_range(0, col.saturating_add(1), fill),
            2 => self.active_row_mut(row).clear_range(0, width, fill),
            _ => {}
        }
        self.bump_content_revision();
    }

    pub(crate) fn erase_chars(&mut self, count: usize) {
        let start = self.cursor.col;
        let end = start.saturating_add(count.max(1)).min(self.width);
        let row = self.cursor_absolute_row();
        let fill = self.erase_style();
        self.active_row_mut(row).clear_range(start, end, fill);
        self.bump_content_revision();
    }

    pub(crate) fn insert_blank_chars(&mut self, count: usize) {
        let count = count.max(1).min(self.width.saturating_sub(self.cursor.col));
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        let fill = self.erase_style();
        let mut cells = self.active_row_mut(row).visual_cells(width);
        for index in (col..width).rev() {
            cells[index] = if index >= col.saturating_add(count) {
                cells[index - count].clone()
            } else {
                Cell::blank(fill)
            };
        }
        self.replace_active_row(row, cells);
        self.bump_content_revision();
    }

    pub(crate) fn delete_chars(&mut self, count: usize) {
        let count = count.max(1).min(self.width.saturating_sub(self.cursor.col));
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        let fill = self.erase_style();
        let mut cells = self.active_row_mut(row).visual_cells(width);
        for index in col..width {
            cells[index] = if index + count < width {
                cells[index + count].clone()
            } else {
                Cell::blank(fill)
            };
        }
        self.replace_active_row(row, cells);
        self.bump_content_revision();
    }

    pub(crate) fn insert_blank_lines(&mut self, count: usize) {
        let (_, bottom) = self.effective_scroll_region();
        if self.cursor.row > bottom {
            return;
        }
        self.scroll_region_down(self.cursor.row, bottom, count.max(1));
        self.bump_content_revision();
    }

    pub(crate) fn delete_lines(&mut self, count: usize) {
        let (_, bottom) = self.effective_scroll_region();
        if self.cursor.row > bottom {
            return;
        }
        self.scroll_region_up(self.cursor.row, bottom, count.max(1));
        self.bump_content_revision();
    }

    pub(crate) fn set_graphic_rendition(&mut self, params: &[i64]) {
        let params = if params.is_empty() {
            vec![0]
        } else {
            params.to_vec()
        };
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.current_style = Style::default(),
                1 => self.current_style.bold = true,
                2 => self.current_style.dim = true,
                3 => self.current_style.italic = true,
                4 => self.current_style.underline = true,
                7 => self.current_style.inverse = true,
                9 => self.current_style.strike = true,
                22 => {
                    self.current_style.bold = false;
                    self.current_style.dim = false;
                }
                23 => self.current_style.italic = false,
                24 => self.current_style.underline = false,
                27 => self.current_style.inverse = false,
                29 => self.current_style.strike = false,
                30..=37 => self.current_style.fg = indexed_color(params[i] - 30),
                39 => self.current_style.fg = None,
                40..=47 => self.current_style.bg = indexed_color(params[i] - 40),
                49 => self.current_style.bg = None,
                90..=97 => self.current_style.fg = indexed_color(params[i] - 90 + 8),
                100..=107 => self.current_style.bg = indexed_color(params[i] - 100 + 8),
                38 | 48 => {
                    let target_fg = params[i] == 38;
                    if let Some((color, consumed)) = parse_extended_color(&params[i + 1..]) {
                        if target_fg {
                            self.current_style.fg = Some(color);
                        } else {
                            self.current_style.bg = Some(color);
                        }
                        i += consumed;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.bump_revision();
    }

    #[must_use]
    pub(crate) const fn current_style(&self) -> Style {
        self.current_style
    }

    #[must_use]
    pub(crate) const fn saved_cursor(&self) -> Cursor {
        self.saved_cursor
    }

    #[must_use]
    pub(crate) const fn saved_pending_wrap(&self) -> bool {
        self.saved_pending_wrap
    }

    #[must_use]
    pub(crate) const fn autowrap(&self) -> bool {
        self.autowrap
    }

    #[must_use]
    pub(crate) const fn pending_wrap(&self) -> bool {
        self.pending_wrap
    }

    #[must_use]
    pub(crate) const fn scroll_region(&self) -> Option<(usize, usize)> {
        self.scroll_region
    }

    #[must_use]
    pub fn viewport_rows(&self) -> Vec<PhysicalRow> {
        match self.mode {
            GridMode::Main => self.main_rows.iter().cloned().collect(),
            GridMode::Alternate => self.alt_rows.clone(),
        }
    }

    #[must_use]
    pub fn viewport_row_ref(&self, row: usize) -> Option<&PhysicalRow> {
        match self.mode {
            GridMode::Main => self.main_rows.get(row),
            GridMode::Alternate => self.alt_rows.get(row),
        }
    }

    pub(crate) fn display_rows_unpadded(
        &self,
        scrollback_offset: usize,
        rows: usize,
    ) -> Vec<PhysicalRow> {
        let requested_rows = rows.max(self.height);
        match self.mode {
            GridMode::Main => self.main_display_rows(scrollback_offset, requested_rows),
            GridMode::Alternate => self.alt_display_rows(scrollback_offset, requested_rows),
        }
    }

    #[must_use]
    pub fn display_window(&self, window: GridRowWindow) -> ProjectedRows {
        let requested_rows = window.rows.max(self.height);
        let mut display_rows = self.display_rows_unpadded(window.scrollback_offset, requested_rows);
        let has_more_above =
            window.scrollback_offset.saturating_add(display_rows.len()) < self.display_row_count();
        if display_rows.len() < requested_rows {
            let missing = requested_rows.saturating_sub(display_rows.len());
            display_rows.resize_with(requested_rows, PhysicalRow::new);
            display_rows.rotate_right(missing);
        }
        ProjectedRows {
            rows: display_rows,
            has_more_above,
        }
    }

    #[must_use]
    pub fn display_rows(&self, scrollback_offset: usize, rows: usize) -> Vec<PhysicalRow> {
        self.display_window(GridRowWindow {
            scrollback_offset,
            rows,
        })
        .rows
    }

    #[must_use]
    pub fn scrollback_rows_hint(&self) -> usize {
        match self.mode {
            GridMode::Main => self
                .history_projected_row_count()
                .saturating_add(projected_pending_row_count(
                    &self.pending_history_cells,
                    self.width,
                ))
                .saturating_add(self.main_rows.len().saturating_sub(self.height)),
            GridMode::Alternate => 0,
        }
    }

    #[must_use]
    pub fn main_row_count(&self) -> usize {
        self.history_projected_row_count()
            .saturating_add(projected_pending_row_count(
                &self.pending_history_cells,
                self.width,
            ))
            .saturating_add(self.main_rows.len())
    }

    fn display_row_count(&self) -> usize {
        match self.mode {
            GridMode::Main => self.main_row_count(),
            GridMode::Alternate => self.alt_rows.len(),
        }
    }

    #[must_use]
    pub fn max_scrollback_offset(&self) -> usize {
        self.display_row_count().saturating_sub(self.height)
    }

    /// Materialize all retained main-screen rows.
    ///
    /// This is intentionally named as a slow path. Production render and resize
    /// code should use [`Self::display_rows`] with a bounded row count instead.
    #[must_use]
    pub fn all_main_rows_slow(&self) -> Vec<PhysicalRow> {
        self.display_rows(0, self.main_row_count())
    }

    /// Materialize retained main-screen content rows without viewport padding.
    ///
    /// The returned rows include retained scrollback and the live viewport up to
    /// the cursor/content extent, but exclude unused terminal rectangle rows.
    /// This is intended for transcript-style renderers that need terminal
    /// fidelity without fixed-pane padding.
    #[must_use]
    pub fn main_content_rows(&self) -> Vec<PhysicalRow> {
        let mut rows = self.main_content_rows_unbounded();
        trim_trailing_unused_rows(&mut rows);
        rows
    }

    /// Materialize the tail of retained main-screen content rows.
    #[must_use]
    pub fn main_content_tail_rows(&self, max_rows: usize) -> Vec<PhysicalRow> {
        if max_rows == 0 {
            return Vec::new();
        }
        let mut offset = 0;
        loop {
            let mut rows = self.main_display_rows(offset, max_rows);
            let before = rows.len();
            trim_trailing_unused_rows(&mut rows);
            let removed = before - rows.len();
            if removed == 0 || before == 0 {
                return rows;
            }
            offset = offset.saturating_add(removed);
            if !rows.is_empty() {
                return self.main_display_rows(offset, max_rows);
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self, scrollback_offset: usize, rows: usize) -> GridSnapshot {
        GridSnapshot::from_grid(self, scrollback_offset, rows)
    }

    pub(crate) fn reset(&mut self) {
        let width = u16::try_from(self.width).unwrap_or(u16::MAX);
        let height = u16::try_from(self.height).unwrap_or(u16::MAX);
        if let Ok(mut reset) = Self::new(width, height, self.limits) {
            reset.revision = self.revision;
            reset.content_revision = self.content_revision;
            reset.bump_content_revision();
            *self = reset;
        }
    }

    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    pub(crate) fn bump_content_revision(&mut self) {
        self.content_revision = self.content_revision.saturating_add(1);
        self.bump_revision();
    }

    const fn cursor_absolute_row(&self) -> usize {
        self.cursor.row
    }

    fn active_row_mut(&mut self, absolute_row: usize) -> &mut PhysicalRow {
        match self.mode {
            GridMode::Main => &mut self.main_rows[absolute_row],
            GridMode::Alternate => &mut self.alt_rows[absolute_row],
        }
    }

    fn replace_active_row(&mut self, absolute_row: usize, cells: Vec<Cell>) {
        let cells = normalized_visual_cells(cells, self.width);
        let mut row = PhysicalRow::new();
        for (col, cell) in cells.into_iter().enumerate() {
            row.set_cell(col, cell);
        }
        *self.active_row_mut(absolute_row) = row;
    }

    const fn viewport_absolute_row(row: usize) -> usize {
        row
    }

    fn viewport_row(&self, row: usize) -> PhysicalRow {
        match self.mode {
            GridMode::Main => self.main_rows[Self::viewport_absolute_row(row)].clone(),
            GridMode::Alternate => self.alt_rows[row].clone(),
        }
    }

    fn set_viewport_row(&mut self, row: usize, value: PhysicalRow) {
        let absolute = Self::viewport_absolute_row(row);
        *self.active_row_mut(absolute) = value;
    }

    /// Interned style used to fill erased or newly exposed cells (BCE).
    fn erase_style(&mut self) -> StyleId {
        let fill = self.current_style.erase_fill();
        if fill == Style::default() {
            return StyleId::DEFAULT;
        }
        self.palette.intern(fill)
    }

    fn fill_viewport_row(&mut self, row: usize, style: StyleId) {
        self.set_viewport_row(row, PhysicalRow::erase_filled(self.width, style));
    }

    fn effective_scroll_region(&self) -> (usize, usize) {
        self.scroll_region
            .unwrap_or((0, self.height.saturating_sub(1)))
    }

    pub(crate) fn scroll_region_up(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= bottom || bottom >= self.height {
            return;
        }
        let fill = self.erase_style();
        for _ in 0..count.min(bottom - top + 1) {
            for row in top..bottom {
                let next = self.viewport_row(row + 1);
                self.set_viewport_row(row, next);
            }
            self.fill_viewport_row(bottom, fill);
        }
    }

    pub(crate) fn scroll_region_down(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= bottom || bottom >= self.height {
            return;
        }
        let fill = self.erase_style();
        for _ in 0..count.min(bottom - top + 1) {
            for row in (top + 1..=bottom).rev() {
                let previous = self.viewport_row(row - 1);
                self.set_viewport_row(row, previous);
            }
            self.fill_viewport_row(top, fill);
        }
    }

    fn scroll_up_one(&mut self) {
        let fill = self.erase_style();
        match self.mode {
            GridMode::Main => {
                if let Some(row) = self.main_rows.pop_front() {
                    self.push_history_row(&row);
                }
                self.main_rows
                    .push_back(PhysicalRow::erase_filled(self.width, fill));
                self.total_scrolled_rows = self.total_scrolled_rows.saturating_add(1);
                self.evict_excess_history();
            }
            GridMode::Alternate => {
                if !self.alt_rows.is_empty() {
                    self.alt_rows.remove(0);
                    self.alt_rows
                        .push(PhysicalRow::erase_filled(self.width, fill));
                }
            }
        }
    }

    fn mark_current_row_wrapped(&mut self) {
        let row = self.cursor_absolute_row();
        self.active_row_mut(row).set_wrapped(true);
    }

    /// Whether content before the retained history is unavailable. Legacy
    /// snapshots conservatively report truncation when provenance is unknown.
    #[must_use]
    pub const fn history_truncated(&self) -> bool {
        self.history_truncated
    }

    /// Number of logical lines addressable by `history_slice` in this capture.
    /// Includes the pending open line when present; unrelated to projected rows.
    #[must_use]
    pub fn history_line_count(&self) -> usize {
        self.main_history.len() + usize::from(!self.pending_history_cells.is_empty())
    }

    /// Read a bounded slice without allocating or projecting physical rows.
    ///
    /// The caller must bind `revision` and `line_index` to one capture identity;
    /// revisions alone do not identify different grids. Completed retained lines
    /// precede the optional pending line. Evicted lines cannot be reconstructed.
    ///
    /// # Errors
    /// Rejects stale revisions, unavailable lines, offsets inside wide cells,
    /// and budgets that cannot make progress. `max_text_bytes` counts source
    /// UTF-8 only, not serialized metadata or envelope overhead.
    pub fn history_slice(
        &self,
        revision: u64,
        line_index: usize,
        cell_offset: usize,
        max_columns: usize,
        max_text_bytes: usize,
    ) -> Result<HistorySlice<'_>, HistorySliceError> {
        if revision != self.revision {
            return Err(HistorySliceError::StaleRevision);
        }
        let (cells, terminal_end) = if let Some(line) = self.main_history.get(line_index) {
            (line.cells.as_slice(), HistorySliceEnd::HardBreak)
        } else if line_index == self.main_history.len() && !self.pending_history_cells.is_empty() {
            (self.pending_history_cells.as_slice(), HistorySliceEnd::Open)
        } else {
            return Err(HistorySliceError::Unavailable);
        };
        let mut offset = 0_usize;
        let mut start = 0;
        while start < cells.len() && offset < cell_offset {
            offset = offset
                .checked_add(usize::from(cells[start].width()))
                .ok_or(HistorySliceError::InvalidOffset)?;
            start += 1;
        }
        if offset != cell_offset {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut end = start;
        let mut columns = 0_usize;
        let mut bytes = 0_usize;
        for cell in &cells[start..] {
            let Some(next_columns) = columns.checked_add(usize::from(cell.width())) else {
                break;
            };
            let Some(next_bytes) = bytes.checked_add(cell.text().len()) else {
                break;
            };
            if next_columns > max_columns || next_bytes > max_text_bytes {
                break;
            }
            columns = next_columns;
            bytes = next_bytes;
            end += 1;
        }
        if end == start && start < cells.len() {
            return Err(HistorySliceError::BudgetExhausted);
        }
        Ok(HistorySlice {
            cells: &cells[start..end],
            next_cell_offset: offset + columns,
            end: if end == cells.len() {
                terminal_end
            } else {
                HistorySliceEnd::Continue
            },
        })
    }

    /// Read a bounded fragment of an immutable main-screen row, even while
    /// the alternate screen is active. `Open` means the row soft-wraps into
    /// the next row; `Continue` means another slice of this row is required.
    /// Physical row boundaries and implicit trailing blanks remain significant:
    /// callers join wrapped rows using the capture width, not slice length.
    ///
    /// # Errors
    /// Rejects stale revisions, missing rows, wide-continuation offsets and
    /// budgets too small to return the next complete cell.
    pub fn main_row_slice(
        &self,
        revision: u64,
        row_index: usize,
        cell_offset: usize,
        max_columns: usize,
        max_text_bytes: usize,
    ) -> Result<HistorySlice<'_>, HistorySliceError> {
        if revision != self.revision {
            return Err(HistorySliceError::StaleRevision);
        }
        let row = self
            .main_rows
            .get(row_index)
            .ok_or(HistorySliceError::Unavailable)?;
        let cells = row.cells();
        if cell_offset > cells.len()
            || cells
                .get(cell_offset)
                .is_some_and(Cell::is_wide_continuation)
        {
            return Err(HistorySliceError::InvalidOffset);
        }
        let mut end = cell_offset;
        let mut bytes = 0_usize;
        while let Some(cell) = cells.get(end) {
            let span = usize::from(cell.width()).max(1).min(cells.len() - end);
            let next = end + span;
            let next_bytes = bytes
                .checked_add(cell.text().len())
                .ok_or(HistorySliceError::BudgetExhausted)?;
            if next - cell_offset > max_columns || next_bytes > max_text_bytes {
                break;
            }
            end = next;
            bytes = next_bytes;
        }
        if end == cell_offset && end < cells.len() {
            return Err(HistorySliceError::BudgetExhausted);
        }
        Ok(HistorySlice {
            cells: &cells[cell_offset..end],
            next_cell_offset: end,
            end: if end < cells.len() {
                HistorySliceEnd::Continue
            } else if row.wrapped() {
                HistorySliceEnd::Open
            } else {
                HistorySliceEnd::HardBreak
            },
        })
    }

    fn push_history_row(&mut self, row: &PhysicalRow) {
        let cells = row_logical_cells(row, self.width);
        self.history_bytes = self
            .history_bytes
            .saturating_add(history_cells_bytes(&cells));
        self.pending_history_cells.extend(cells);
        if !row.wrapped() {
            let pending = std::mem::take(&mut self.pending_history_cells);
            self.history_bytes = self
                .history_bytes
                .saturating_sub(history_cells_bytes(&pending));
            let cells = trim_trailing_blank_cells(pending);
            self.push_history_line(LogicalLine::new(cells));
        }
    }

    fn push_history_line(&mut self, line: LogicalLine) {
        self.history_bytes = self
            .history_bytes
            .saturating_add(history_cells_bytes(&line.cells));
        if let Some(count) = self.main_history_projected_rows.get_mut() {
            *count = count.saturating_add(line.projected_row_count(self.width));
        }
        self.main_history.push_back(line);
    }

    fn history_projected_row_count(&self) -> usize {
        *self.main_history_projected_rows.get_or_init(|| {
            self.main_history
                .iter()
                .map(|line| line.projected_row_count(self.width))
                .sum()
        })
    }

    #[cfg(test)]
    pub(crate) fn try_display_rows(
        &self,
        offset: usize,
        rows: usize,
        main: bool,
    ) -> Option<Vec<PhysicalRow>> {
        let mut remaining = usize::MAX;
        self.try_display_rows_charged(offset, rows, main, &mut remaining)
    }

    pub(crate) fn try_display_rows_charged(
        &self,
        offset: usize,
        rows: usize,
        main: bool,
        remaining: &mut usize,
    ) -> Option<Vec<PhysicalRow>> {
        let mut selected = Vec::new();
        if !main && self.mode == GridMode::Alternate {
            let end = self.alt_rows.len().saturating_sub(offset);
            let start = end.saturating_sub(rows);
            crate::snapshot::reserve_snapshot_vec(&mut selected, end - start, remaining)?;
            for row in &self.alt_rows[start..end] {
                selected.push(try_clone_row_charged(row, remaining)?);
            }
            return Some(selected);
        }
        let mut skip = offset;
        crate::snapshot::reserve_snapshot_vec(
            &mut selected,
            rows.min(self.main_row_count().saturating_sub(offset)),
            remaining,
        )?;
        for row in self.main_rows.iter().rev() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if selected.len() == rows {
                break;
            }
            selected.push(try_clone_row_charged(row, remaining)?);
        }
        let lines = std::iter::once((self.pending_history_cells.as_slice(), true))
            .filter(|(cells, _)| !cells.is_empty())
            .chain(
                self.main_history
                    .iter()
                    .rev()
                    .map(|line| (line.cells.as_slice(), false)),
            );
        for (cells, pending) in lines {
            if selected.len() == rows {
                break;
            }
            let count = projected_row_count(cells, self.width);
            if skip >= count {
                skip -= count;
                continue;
            }
            let end = count - skip;
            let start = end.saturating_sub(rows - selected.len());
            // Admit selected cells plus both the destination vector (already
            // charged) and the temporary projection deque before projection.
            let metadata = (end - start)
                .checked_mul(std::mem::size_of::<PhysicalRow>())?
                .checked_add(crate::reflow::projected_cell_storage(
                    cells,
                    self.width,
                    start..end,
                )?)?;
            let mut text_budget = remaining.checked_sub(metadata)?;
            crate::reflow::admit_logical_text(cells, self.width, start..end, &mut text_budget)?;
            let projected =
                crate::reflow::try_project_logical_line_window(cells, self.width, start..end)?;
            let deque_bytes = projected
                .capacity()
                .checked_mul(std::mem::size_of::<PhysicalRow>())?;
            let allocated = projected.iter().try_fold(deque_bytes, |bytes, row| {
                bytes.checked_add(row.allocated_bytes()?)
            })?;
            *remaining = remaining.checked_sub(allocated)?;
            for mut row in projected.into_iter().rev() {
                if pending {
                    row.set_wrapped(true);
                }
                selected.push(row);
            }
            *remaining = remaining.checked_add(deque_bytes)?;
            skip = 0;
        }
        selected.reverse();
        Some(selected)
    }

    pub(crate) fn main_display_rows(
        &self,
        scrollback_offset: usize,
        requested_rows: usize,
    ) -> Vec<PhysicalRow> {
        if requested_rows == 0 {
            return Vec::new();
        }
        let mut skipped = 0_usize;
        let mut selected = Vec::with_capacity(requested_rows.min(self.height.saturating_mul(2)));
        self.collect_main_rows_reversed(
            scrollback_offset,
            requested_rows,
            &mut skipped,
            &mut selected,
        );
        selected.reverse();
        selected
    }

    fn main_content_rows_unbounded(&self) -> Vec<PhysicalRow> {
        let mut skipped = 0_usize;
        let mut selected = Vec::with_capacity(self.main_row_count());
        self.collect_main_rows_reversed(0, usize::MAX, &mut skipped, &mut selected);
        selected.reverse();
        selected
    }

    fn collect_main_rows_reversed(
        &self,
        scrollback_offset: usize,
        requested_rows: usize,
        skipped: &mut usize,
        selected: &mut Vec<PhysicalRow>,
    ) {
        let skip_live = scrollback_offset
            .saturating_sub(*skipped)
            .min(self.main_rows.len());
        *skipped = skipped.saturating_add(skip_live);
        let end = self.main_rows.len() - skip_live;
        let start = end.saturating_sub(requested_rows.saturating_sub(selected.len()));
        selected.extend(self.main_rows.range(start..end).rev().cloned());
        if selected.len() < requested_rows && !self.pending_history_cells.is_empty() {
            let pending_rows = projected_row_count(&self.pending_history_cells, self.width);
            let selected_start = selected.len();
            collect_projected_line_reversed(
                &self.pending_history_cells,
                pending_rows,
                self.width,
                scrollback_offset,
                requested_rows,
                skipped,
                selected,
            );
            // Every pending-history row continues, including its final row
            // into the live rows. A clipped window must not create a hard break.
            for row in &mut selected[selected_start..] {
                row.set_wrapped(true);
            }
        }
        for line in self.main_history.iter().rev() {
            if selected.len() >= requested_rows {
                break;
            }
            collect_projected_line_reversed(
                &line.cells,
                line.projected_row_count(self.width),
                self.width,
                scrollback_offset,
                requested_rows,
                skipped,
                selected,
            );
        }
    }

    fn alt_display_rows(
        &self,
        scrollback_offset: usize,
        requested_rows: usize,
    ) -> Vec<PhysicalRow> {
        let total_rows = self.alt_rows.len();
        let end = total_rows.saturating_sub(scrollback_offset.min(total_rows));
        let start = end.saturating_sub(requested_rows);
        self.alt_rows[start..end].to_vec()
    }

    fn resize_main_viewport(
        &mut self,
        new_width: usize,
        new_height: usize,
        main_cursor: Cursor,
    ) -> Cursor {
        let mut source_rows = std::mem::take(&mut self.main_rows)
            .into_iter()
            .collect::<Vec<_>>();
        while source_rows.len() > 1
            && source_rows
                .last()
                .is_some_and(|row| row_is_blank(row) && !row.wrapped())
            && main_cursor.row < source_rows.len().saturating_sub(1)
        {
            source_rows.pop();
        }
        let anchor = self.live_cursor_anchor(&source_rows, main_cursor);
        let live_lines = self.live_logical_lines(&source_rows);
        let projected_counts = live_lines
            .iter()
            .map(|line| projected_row_count(&line.cells, new_width))
            .collect::<Vec<_>>();
        let total_rows = projected_counts
            .iter()
            .fold(0_usize, |total, count| total.saturating_add(*count));
        let keep_start = total_rows.saturating_sub(new_height);
        let mut row_index = 0_usize;
        let mut next_pending = Vec::new();
        let mut next_rows = VecDeque::new();
        for (line, count) in live_lines.into_iter().zip(&projected_counts) {
            let line_start = row_index;
            let line_end = line_start.saturating_add(*count);
            if line_end <= keep_start {
                self.push_history_line(line);
            } else {
                let hidden = keep_start.saturating_sub(line_start);
                for (index, row) in project_logical_line(&line.cells, new_width)
                    .into_iter()
                    .enumerate()
                {
                    if index < hidden {
                        next_pending.extend(row_logical_cells(&row, new_width));
                    } else {
                        next_rows.push_back(row);
                    }
                }
            }
            row_index = line_end;
        }
        self.pending_history_cells = trim_trailing_blank_cells(next_pending);
        self.history_bytes = self
            .main_history
            .iter()
            .map(|line| history_cells_bytes(&line.cells))
            .sum::<usize>()
            .saturating_add(history_cells_bytes(&self.pending_history_cells));
        while next_rows.len() < new_height {
            next_rows.push_back(PhysicalRow::new());
        }
        while next_rows.len() > new_height {
            if let Some(row) = next_rows.pop_front() {
                self.push_history_row(&row);
            }
        }
        self.main_rows = next_rows;
        Self::restored_live_cursor_anchor(
            main_cursor,
            anchor,
            &projected_counts,
            keep_start,
            new_width,
            new_height,
        )
    }

    fn live_cursor_anchor(
        &self,
        source_rows: &[PhysicalRow],
        main_cursor: Cursor,
    ) -> Option<CursorAnchor> {
        if source_rows.is_empty() {
            return None;
        }
        let mut logical_line = 0_usize;
        let mut run_start = 0_usize;
        let mut prefix_cols = logical_width(&self.pending_history_cells);
        for (index, row) in source_rows.iter().enumerate() {
            if index == main_cursor.row.min(source_rows.len().saturating_sub(1)) {
                return Some(CursorAnchor {
                    logical_line,
                    logical_col: prefix_cols
                        .saturating_add(index.saturating_sub(run_start).saturating_mul(self.width))
                        .saturating_add(main_cursor.col),
                });
            }
            if !row.wrapped() {
                logical_line = logical_line.saturating_add(1);
                run_start = index.saturating_add(1);
                prefix_cols = 0;
            }
        }
        None
    }

    fn live_logical_lines(&mut self, source_rows: &[PhysicalRow]) -> Vec<LogicalLine> {
        let mut lines = Vec::new();
        let mut logical = std::mem::take(&mut self.pending_history_cells);
        for row in source_rows {
            logical.extend(row_logical_cells(row, self.width));
            if !row.wrapped() {
                lines.push(LogicalLine::new(trim_trailing_blank_cells(std::mem::take(
                    &mut logical,
                ))));
            }
        }
        if !logical.is_empty() {
            lines.push(LogicalLine::new(trim_trailing_blank_cells(logical)));
        }
        if lines.is_empty() {
            lines.push(LogicalLine::default());
        }
        lines
    }

    fn restored_live_cursor_anchor(
        fallback_cursor: Cursor,
        anchor: Option<CursorAnchor>,
        projected_counts: &[usize],
        keep_start: usize,
        width: usize,
        height: usize,
    ) -> Cursor {
        let Some(anchor) = anchor else {
            return clamp_cursor_to_dimensions(fallback_cursor, width, height);
        };
        let mut absolute_row = 0_usize;
        for (line_index, count) in projected_counts.iter().enumerate() {
            if line_index == anchor.logical_line {
                absolute_row = absolute_row.saturating_add(anchor.logical_col / width.max(1));
                return Cursor {
                    row: absolute_row
                        .saturating_sub(keep_start)
                        .min(height.saturating_sub(1)),
                    col: (anchor.logical_col % width.max(1)).min(width.saturating_sub(1)),
                    visible: fallback_cursor.visible,
                };
            }
            absolute_row = absolute_row.saturating_add(*count);
        }
        clamp_cursor_to_dimensions(fallback_cursor, width, height)
    }

    fn append_combining(&mut self, ch: char) {
        let row = self.cursor_absolute_row();
        let col = self.cursor.col.saturating_sub(1);
        if let Some(cell) = self.active_row_mut(row).cell_mut(col)
            && !cell.is_wide_continuation()
        {
            cell.append_combining(ch);
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.height.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.width.saturating_sub(1));
    }

    /// Whether either screen contains a cell whose text exceeds a byte limit.
    ///
    /// This borrows existing storage and performs no projection or text cloning.
    /// History is deliberately excluded: a viewport snapshot need not carry it.
    #[must_use]
    pub fn screen_cell_text_exceeds(&self, max_bytes: usize) -> bool {
        self.main_rows
            .iter()
            .chain(self.alt_rows.iter())
            .any(|row| row.cells().iter().any(|cell| cell.text().len() > max_bytes))
    }

    /// Admit aggregate source text for exactly the snapshot's selected rows.
    /// No rows or cell text are cloned. This is a text bound, not a bound on
    /// metadata, parser state, or allocator overhead.
    #[must_use]
    pub fn snapshot_text_fits(&self, offset: usize, rows: usize, max_bytes: usize) -> bool {
        let rows = if rows == usize::MAX {
            self.height
        } else {
            rows.max(self.height)
        };
        let mut remaining = max_bytes;
        let mut admit_main = |offset: usize| -> Option<()> {
            let mut skip = offset;
            let mut needed = rows;
            for row in self.main_rows.iter().rev() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if needed == 0 {
                    break;
                }
                for cell in row.cells() {
                    remaining = remaining.checked_sub(cell.text().len())?;
                }
                needed -= 1;
            }
            let lines = std::iter::once(self.pending_history_cells.as_slice())
                .filter(|cells| !cells.is_empty())
                .chain(
                    self.main_history
                        .iter()
                        .rev()
                        .map(|line| line.cells.as_slice()),
                );
            for cells in lines {
                if needed == 0 {
                    break;
                }
                let count = projected_row_count(cells, self.width);
                if skip >= count {
                    skip -= count;
                    continue;
                }
                let end = count - skip;
                let start = end.saturating_sub(needed);
                crate::reflow::admit_logical_text(cells, self.width, start..end, &mut remaining)?;
                needed -= end - start;
                skip = 0;
            }
            Some(())
        };
        if admit_main(if self.mode == GridMode::Main {
            offset
        } else {
            0
        })
        .is_none()
        {
            return false;
        }
        if self.mode == GridMode::Alternate {
            let end = self.alt_rows.len().saturating_sub(offset);
            for row in &self.alt_rows[end.saturating_sub(rows)..end] {
                for cell in row.cells() {
                    let Some(next) = remaining.checked_sub(cell.text().len()) else {
                        return false;
                    };
                    remaining = next;
                }
            }
        }
        true
    }

    /// Clone retained state only after admitting its owned payload. Container
    /// and text reservations are fallible; allocator bookkeeping is excluded.
    pub fn try_clone_with_budget(&self, mut max_bytes: usize) -> Option<Self> {
        self.try_clone_charged(&mut max_bytes)
    }

    /// Clone and debit a shared payload budget only on successful allocation.
    /// The remaining budget can admit temporary storage coexisting with this clone.
    pub fn try_clone_charged(&self, budget: &mut usize) -> Option<Self> {
        let mut remaining = budget.checked_sub(std::mem::size_of::<Self>())?;
        let mut charge = |count: usize, size: usize| -> Option<()> {
            remaining = remaining.checked_sub(count.checked_mul(size)?)?;
            Some(())
        };
        charge(self.main_history.len(), std::mem::size_of::<LogicalLine>())?;
        charge(self.main_rows.len(), std::mem::size_of::<PhysicalRow>())?;
        charge(self.alt_rows.len(), std::mem::size_of::<PhysicalRow>())?;
        charge(self.palette.styles().len(), std::mem::size_of::<Style>())?;
        let cells = self
            .main_history
            .iter()
            .map(|line| line.cells.as_slice())
            .chain(std::iter::once(self.pending_history_cells.as_slice()))
            .chain(
                self.main_rows
                    .iter()
                    .chain(self.alt_rows.iter())
                    .map(PhysicalRow::cells),
            );
        for row in cells {
            charge(row.len(), std::mem::size_of::<Cell>())?;
            for cell in row {
                charge(cell.text.len(), 1)?;
            }
        }
        let mut main_history = VecDeque::new();
        main_history
            .try_reserve_exact(self.main_history.len())
            .ok()?;
        for line in &self.main_history {
            main_history.push_back(LogicalLine::new(try_clone_cells(&line.cells)?));
        }
        let mut main_rows = VecDeque::new();
        main_rows.try_reserve_exact(self.main_rows.len()).ok()?;
        for row in &self.main_rows {
            main_rows.push_back(try_clone_row(row)?);
        }
        let mut alt_rows = Vec::new();
        alt_rows.try_reserve_exact(self.alt_rows.len()).ok()?;
        for row in &self.alt_rows {
            alt_rows.push(try_clone_row(row)?);
        }
        let mut styles = Vec::new();
        styles.try_reserve_exact(self.palette.styles().len()).ok()?;
        styles.extend_from_slice(self.palette.styles());
        let cloned = Self {
            width: self.width,
            height: self.height,
            limits: self.limits,
            history_bytes: self.history_bytes,
            history_truncated: self.history_truncated,
            main_history,
            main_history_projected_rows: OnceLock::new(),
            pending_history_cells: try_clone_cells(&self.pending_history_cells)?,
            main_rows,
            alt_rows,
            mode: self.mode,
            cursor: self.cursor,
            saved_cursor: self.saved_cursor,
            saved_pending_wrap: self.saved_pending_wrap,
            characters: self.characters,
            saved_characters: self.saved_characters,
            current_style: self.current_style,
            palette: StylePalette::from_styles(styles),
            revision: self.revision,
            content_revision: self.content_revision,
            total_scrolled_rows: self.total_scrolled_rows,
            autowrap: self.autowrap,
            pending_wrap: self.pending_wrap,
            scroll_region: self.scroll_region,
            protocol: self.protocol,
        };
        // Exact reservations may still receive excess capacity from an allocator.
        // Do not publish that capacity without charging the shared budget.
        *budget = budget.checked_sub(cloned.retained_capacity_bytes()?)?;
        Some(cloned)
    }

    /// Owned allocation capacity plus the grid value, excluding allocator
    /// bookkeeping and allocations owned by callers (for example Arc headers).
    #[must_use]
    pub fn retained_capacity_bytes(&self) -> Option<usize> {
        let mut bytes = std::mem::size_of::<Self>();
        let mut charge = |count: usize, size: usize| -> Option<()> {
            bytes = bytes.checked_add(count.checked_mul(size)?)?;
            Some(())
        };
        charge(
            self.main_history.capacity(),
            std::mem::size_of::<LogicalLine>(),
        )?;
        charge(
            self.main_rows.capacity(),
            std::mem::size_of::<PhysicalRow>(),
        )?;
        charge(self.alt_rows.capacity(), std::mem::size_of::<PhysicalRow>())?;
        charge(self.palette.capacity(), std::mem::size_of::<Style>())?;
        let vectors = self
            .main_history
            .iter()
            .map(|line| &line.cells)
            .chain(std::iter::once(&self.pending_history_cells))
            .chain(
                self.main_rows
                    .iter()
                    .chain(self.alt_rows.iter())
                    .map(|row| &row.cells),
            );
        for cells in vectors {
            charge(cells.capacity(), std::mem::size_of::<Cell>())?;
            for cell in cells {
                charge(cell.text.capacity(), 1)?;
            }
        }
        Some(bytes)
    }

    pub(crate) fn retained_history_line_count(&self) -> usize {
        self.main_history.len()
    }

    fn evict_excess_history(&mut self) {
        self.evict_history_to_budget(8 * 1024 * 1024);
    }

    // Charge stored cell metadata and UTF-8 bytes, not physical projected rows.
    // Evict completed lines first. An oversized open line loses its prefix as
    // a whole; the truncation flag prevents presenting the suffix as complete.
    fn evict_history_to_budget(&mut self, max_bytes: usize) {
        while self.main_history.len() > self.limits.scrollback_rows
            || self.history_bytes > max_bytes
        {
            self.history_truncated = true;
            if let Some(line) = self.main_history.pop_front() {
                self.history_bytes = self
                    .history_bytes
                    .saturating_sub(history_cells_bytes(&line.cells));
                if let Some(count) = self.main_history_projected_rows.get_mut() {
                    *count = count.saturating_sub(line.projected_row_count(self.width));
                }
            } else {
                self.pending_history_cells = Vec::new();
                self.history_bytes = 0;
                break;
            }
        }
    }
}

fn history_cells_bytes(cells: &[Cell]) -> usize {
    cells.iter().fold(
        cells.len().saturating_mul(std::mem::size_of::<Cell>()),
        |bytes, cell| bytes.saturating_add(cell.text.len()),
    )
}

fn try_clone_cells(cells: &[Cell]) -> Option<Vec<Cell>> {
    let mut cloned = Vec::new();
    cloned.try_reserve_exact(cells.len()).ok()?;
    for cell in cells {
        let mut text = String::new();
        text.try_reserve_exact(cell.text.len()).ok()?;
        text.push_str(&cell.text);
        cloned.push(Cell {
            text,
            style: cell.style,
            width: cell.width,
            wide_continuation: cell.wide_continuation,
        });
    }
    Some(cloned)
}

fn try_clone_row_charged(row: &PhysicalRow, remaining: &mut usize) -> Option<PhysicalRow> {
    let bytes = row.cells.iter().try_fold(
        row.cells.len().checked_mul(std::mem::size_of::<Cell>())?,
        |bytes, cell| bytes.checked_add(cell.text.len()),
    )?;
    remaining.checked_sub(bytes)?;
    let cloned = try_clone_row(row)?;
    *remaining = remaining.checked_sub(cloned.allocated_bytes()?)?;
    Some(cloned)
}

fn try_clone_row(row: &PhysicalRow) -> Option<PhysicalRow> {
    Some(PhysicalRow {
        cells: try_clone_cells(&row.cells)?,
        wrapped: row.wrapped,
    })
}

fn clamp_cursor_to_dimensions(mut cursor: Cursor, width: usize, height: usize) -> Cursor {
    cursor.row = cursor.row.min(height.saturating_sub(1));
    cursor.col = cursor.col.min(width.saturating_sub(1));
    cursor
}

fn normalized_visual_cells(mut cells: Vec<Cell>, width: usize) -> Vec<Cell> {
    cells.resize_with(width, || Cell::blank(StyleId::DEFAULT));
    cells.truncate(width);
    let mut col = 0;
    while col < width {
        if cells[col].is_wide_continuation() {
            let valid_previous =
                col > 0 && !cells[col - 1].is_wide_continuation() && cells[col - 1].width() == 2;
            if !valid_previous {
                cells[col] = Cell::blank(StyleId::DEFAULT);
            }
            col += 1;
            continue;
        }
        if cells[col].width() == 2 {
            if col + 1 >= width {
                cells[col] = Cell::blank(StyleId::DEFAULT);
            } else {
                let style = cells[col].style();
                cells[col + 1] = Cell::spacer(style);
                col += 2;
                continue;
            }
        }
        col += 1;
    }
    cells
}

fn row_from_snapshot(snapshot: &RowSnapshot, width: usize) -> PhysicalRow {
    let mut row = PhysicalRow::new();
    row.set_wrapped(snapshot.wrapped);
    for run in &snapshot.runs {
        let mut col = usize::from(run.start_col).min(width.saturating_sub(1));
        for ch in run.text.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0).min(2);
            if char_width == 0 {
                if col > 0
                    && let Some(cell) = row.cell_mut(col - 1)
                    && !cell.is_wide_continuation()
                {
                    cell.append_combining(ch);
                }
                continue;
            }
            if col >= width {
                break;
            }
            row.set_cell(
                col,
                Cell::new(
                    ch.to_string(),
                    run.style,
                    u8::try_from(char_width).unwrap_or(1),
                ),
            );
            if char_width == 2 && col + 1 < width {
                row.set_cell(col + 1, Cell::spacer(run.style));
            }
            col = col.saturating_add(char_width);
        }
    }
    row
}

fn hydrate_logical_history(
    rows: &[PhysicalRow],
    width: usize,
    history: &mut VecDeque<LogicalLine>,
    pending: &mut Vec<Cell>,
) {
    for row in rows {
        pending.extend(row_logical_cells(row, width));
        if !row.wrapped() {
            history.push_back(LogicalLine::new(trim_trailing_blank_cells(std::mem::take(
                pending,
            ))));
        }
    }
}

fn row_logical_cells(row: &PhysicalRow, width: usize) -> Vec<Cell> {
    row.visual_cells(width)
        .into_iter()
        .filter(|cell| !cell.is_wide_continuation())
        .collect()
}

fn trim_trailing_blank_cells(mut cells: Vec<Cell>) -> Vec<Cell> {
    while cells.last().is_some_and(Cell::is_discardable_blank) {
        cells.pop();
    }
    cells
}

fn projected_row_count(cells: &[Cell], width: usize) -> usize {
    projected_logical_line_row_count(cells, width)
}

/// Return whether a physical row contains no visible text.
#[must_use]
pub fn physical_row_is_blank(row: &PhysicalRow) -> bool {
    row.cells()
        .iter()
        .filter(|cell| !cell.is_wide_continuation())
        .all(Cell::is_discardable_blank)
}

fn trim_trailing_unused_rows(rows: &mut Vec<PhysicalRow>) {
    while rows.last().is_some_and(physical_row_is_blank) {
        rows.pop();
    }
}

fn projected_pending_row_count(cells: &[Cell], width: usize) -> usize {
    if cells.is_empty() {
        0
    } else {
        projected_row_count(cells, width)
    }
}

fn logical_width(cells: &[Cell]) -> usize {
    cells
        .iter()
        .map(|cell| usize::from(cell.width()).max(1))
        .sum()
}

fn row_is_blank(row: &PhysicalRow) -> bool {
    row.cells().iter().all(Cell::is_discardable_blank)
}

fn collect_projected_line_reversed(
    cells: &[Cell],
    projected_rows: usize,
    width: usize,
    scrollback_offset: usize,
    requested_rows: usize,
    skipped: &mut usize,
    selected: &mut Vec<PhysicalRow>,
) {
    if skipped.saturating_add(projected_rows) <= scrollback_offset {
        *skipped = skipped.saturating_add(projected_rows);
        return;
    }
    let skip = scrollback_offset
        .saturating_sub(*skipped)
        .min(projected_rows);
    let end = projected_rows.saturating_sub(skip);
    let start = end.saturating_sub(requested_rows.saturating_sub(selected.len()));
    *skipped = skipped.saturating_add(skip);
    let rows = crate::reflow::project_logical_line_window(cells, width, start..end);
    selected.extend(rows.into_iter().rev());
}

fn indexed_color(index: i64) -> Option<Color> {
    Some(Color::Indexed(u8::try_from(index).ok()?))
}

fn parse_extended_color(params: &[i64]) -> Option<(Color, usize)> {
    match params {
        [5, index, ..] => Some((Color::Indexed(u8::try_from(*index).ok()?), 2)),
        [2, r, g, b, ..] => Some((
            Color::Rgb {
                r: u8::try_from(*r).ok()?,
                g: u8::try_from(*g).ok()?,
                b: u8::try_from(*b).ok()?,
            },
            4,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembly_rejects_gaps_identity_changes_and_budget_without_partial_commit() {
        let cells = vec![Cell::new("界", StyleId(0), 2)];
        let mut assembly = HistoryLineAssembly::new(7, 0, history_cells_bytes(&cells), true);
        let slice = HistorySlice {
            cells: &cells,
            next_cell_offset: 2,
            end: HistorySliceEnd::Continue,
        };
        assert_eq!(
            assembly.append(8, 0, 0, &slice),
            Err(HistorySliceError::StaleRevision)
        );
        assert_eq!(
            assembly.append(7, 0, 1, &slice),
            Err(HistorySliceError::InvalidOffset)
        );
        assembly.append(7, 0, 0, &slice).unwrap();
        assert!(assembly.completed().is_none());
        let end = HistorySlice {
            cells: &cells,
            next_cell_offset: 4,
            end: HistorySliceEnd::HardBreak,
        };
        assert_eq!(
            assembly.append(7, 0, 2, &end),
            Err(HistorySliceError::BudgetExhausted)
        );
        assert_eq!(assembly.next_offset(), 2);
        let end = HistorySlice {
            cells: &[],
            next_cell_offset: 2,
            end: HistorySliceEnd::Open,
        };
        assembly.append(7, 0, 2, &end).unwrap();
        assert_eq!(
            assembly.completed(),
            Some((cells.as_slice(), HistorySliceEnd::Open))
        );
        assert!(assembly.prefix_unavailable());
        assert_eq!(
            assembly.append(7, 0, 2, &end),
            Err(HistorySliceError::InvalidOffset)
        );
    }

    #[test]
    fn history_byte_budget_discards_prefix_and_preserves_truncation() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        let style = StyleId(0);
        grid.push_history_line(LogicalLine::new(vec![Cell::new("old", style, 1)]));
        grid.push_history_line(LogicalLine::new(vec![Cell::new("new", style, 1)]));
        let budget = history_cells_bytes(&grid.main_history.back().unwrap().cells);
        grid.evict_history_to_budget(budget);
        assert_eq!(grid.history_line_count(), 1);
        assert_eq!(grid.main_history.front().unwrap().cells[0].text(), "new");
        assert!(grid.history_truncated());
        assert!(grid.history_bytes <= budget);
        let snapshot = GridSnapshot::from_grid(&grid, 0, usize::MAX);
        let restored = TerminalGrid::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        assert!(restored.history_truncated());
        assert!(
            grid.try_clone_charged(&mut (1024 * 1024))
                .unwrap()
                .history_truncated()
        );
        let mut legacy = snapshot;
        legacy.history_truncated = None;
        assert!(
            TerminalGrid::from_snapshot(&legacy, GridLimits::default())
                .unwrap()
                .history_truncated()
        );

        grid.pending_history_cells = vec![Cell::new("oversized", style, 1)];
        grid.history_bytes += history_cells_bytes(&grid.pending_history_cells);
        grid.evict_history_to_budget(1);
        assert_eq!(grid.history_line_count(), 0);
        assert_eq!(grid.pending_history_cells.capacity(), 0);
        assert_eq!(grid.history_bytes, 0);
    }

    #[test]
    fn main_row_slices_preserve_wide_cells_and_wrap_boundaries() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.main_rows[0].set_cell(0, Cell::new("界", StyleId::DEFAULT, 2));
        grid.main_rows[0].set_cell(1, Cell::spacer(StyleId::DEFAULT));
        grid.main_rows[0].set_cell(2, Cell::new("x", StyleId::DEFAULT, 1));
        grid.main_rows[0].set_wrapped(true);
        let revision = grid.revision();
        assert_eq!(
            grid.main_row_slice(revision, 0, 0, 1, 10).unwrap_err(),
            HistorySliceError::BudgetExhausted
        );
        assert_eq!(
            grid.main_row_slice(revision, 0, 1, 4, 10).unwrap_err(),
            HistorySliceError::InvalidOffset
        );
        let first = grid.main_row_slice(revision, 0, 0, 2, 3).unwrap();
        assert_eq!(first.cells.len(), 2);
        assert_eq!(first.next_cell_offset, 2);
        assert_eq!(first.end, HistorySliceEnd::Continue);
        let last = grid.main_row_slice(revision, 0, 2, 2, 1).unwrap();
        assert_eq!(last.end, HistorySliceEnd::Open);
        assert_eq!(last.cells[0].text(), "x");
        assert_eq!(
            grid.main_row_slice(revision, 1, 0, 0, 0).unwrap().end,
            HistorySliceEnd::HardBreak
        );
        assert_eq!(
            grid.main_row_slice(revision.wrapping_add(1), 0, 0, 4, 10)
                .unwrap_err(),
            HistorySliceError::StaleRevision
        );
    }

    #[test]
    fn history_slices_preserve_column_boundaries_and_capture_revision() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.push_history_line(LogicalLine::new(vec![
            Cell::new("界", StyleId::DEFAULT, 2),
            Cell::new("x", StyleId::DEFAULT, 1),
        ]));
        let revision = grid.revision();
        assert!(matches!(
            grid.history_slice(revision, 0, 1, 4, 20),
            Err(HistorySliceError::InvalidOffset)
        ));
        assert!(matches!(
            grid.history_slice(revision, 0, 0, 1, 20),
            Err(HistorySliceError::BudgetExhausted)
        ));
        assert!(matches!(
            grid.history_slice(revision, 0, 0, 2, 2),
            Err(HistorySliceError::BudgetExhausted)
        ));
        let first = grid.history_slice(revision, 0, 0, 2, 3).unwrap();
        assert_eq!(first.cells[0].text(), "界");
        assert_eq!(first.next_cell_offset, 2);
        assert_eq!(first.end, HistorySliceEnd::Continue);
        let last = grid
            .history_slice(revision, 0, first.next_cell_offset, 1, 1)
            .unwrap();
        assert_eq!(last.cells[0].text(), "x");
        assert_eq!(last.end, HistorySliceEnd::HardBreak);
        grid.pending_history_cells
            .push(Cell::new("z", StyleId::DEFAULT, 1));
        assert_eq!(
            grid.history_slice(revision, 1, 0, 1, 1).unwrap().end,
            HistorySliceEnd::Open
        );
        grid.process(b"a");
        assert!(matches!(
            grid.history_slice(revision, 0, 0, 4, 20),
            Err(HistorySliceError::StaleRevision)
        ));
    }

    #[test]
    fn sparse_wide_history_admission_ignores_unused_columns() {
        let mut grid = TerminalGrid::new(1000, 2, GridLimits::default()).unwrap();
        grid.process(b"a\r\nb\r\nc\r\nd");
        let mut budget = 1000;
        let rows = grid
            .try_display_rows_charged(2, 2, true, &mut budget)
            .unwrap();
        assert_eq!(rows, grid.main_display_rows(2, 2));
        assert!(budget > 0);
    }

    #[test]
    fn charged_projection_rejects_before_history_materialization() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdefghijklmnop");
        crate::reflow::reset_projection_stats();
        let mut budget = 2 * std::mem::size_of::<PhysicalRow>();
        assert!(
            grid.try_display_rows_charged(2, 2, true, &mut budget)
                .is_none()
        );
        assert_eq!(crate::reflow::projection_stats().physical_rows_projected, 0);
        let mut budget = 100_000;
        let rows = grid
            .try_display_rows_charged(2, 2, true, &mut budget)
            .unwrap();
        let actual = rows.capacity() * std::mem::size_of::<PhysicalRow>()
            + rows
                .iter()
                .map(|row| row.allocated_bytes().unwrap())
                .sum::<usize>();
        assert_eq!(100_000 - budget, actual);
        assert_eq!(rows, grid.main_display_rows(2, 2));
    }

    #[test]
    fn retained_capacity_accounts_for_unused_storage_and_clone_debit() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"x");
        let initial = grid.retained_capacity_bytes().unwrap();
        let row = grid.main_rows.front_mut().unwrap();
        let old_cells = row.cells.capacity();
        let old_text = row.cells[0].text.capacity();
        row.cells.reserve_exact(100);
        row.cells[0].text.reserve_exact(1000);
        let extra = (row.cells.capacity() - old_cells) * std::mem::size_of::<Cell>()
            + row.cells[0].text.capacity()
            - old_text;
        assert_eq!(grid.retained_capacity_bytes(), Some(initial + extra));
        let mut budget = 100_000;
        let cloned = grid.try_clone_charged(&mut budget).unwrap();
        assert_eq!(100_000 - budget, cloned.retained_capacity_bytes().unwrap());
        assert_eq!(cloned.snapshot(0, 2), grid.snapshot(0, 2));
        assert!(cloned.retained_capacity_bytes().unwrap() < initial + extra);
    }

    #[test]
    fn logical_line_projection_cache_keeps_width_and_count_coherent() {
        let line = LogicalLine::new(vec![Cell::new("x".to_owned(), StyleId::DEFAULT, 1); 120]);
        std::thread::scope(|scope| {
            for width in [2, 3, 5, 8, 12] {
                let line = &line;
                scope.spawn(move || {
                    for _ in 0..1_000 {
                        assert_eq!(line.projected_row_count(width), 120 / width);
                        assert_eq!(line.clone().projected_row_count(width), 120 / width);
                    }
                });
            }
        });
    }

    #[test]
    fn content_revision_ignores_cursor_only_changes() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        let initial_revision = grid.revision();
        let initial_content_revision = grid.content_revision();

        grid.process(b"\x1b[2;4H");

        assert!(grid.revision() > initial_revision);
        assert_eq!(grid.content_revision(), initial_content_revision);
    }

    #[test]
    fn content_revision_tracks_visible_content_changes() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        let initial_content_revision = grid.content_revision();

        grid.process(b"x");
        let printed_content_revision = grid.content_revision();
        assert!(printed_content_revision > initial_content_revision);

        grid.process(b"\x1b[K");
        assert!(grid.content_revision() > printed_content_revision);
    }

    #[test]
    fn main_content_rows_excludes_unused_viewport_padding() {
        let mut grid = TerminalGrid::new(10, 5, GridLimits::default()).unwrap();

        grid.process(b"hello\r\nworld");

        let rows = grid.main_content_rows();
        let text = rows.iter().map(row_text).collect::<Vec<_>>();
        assert_eq!(text, vec!["hello", "world"]);
    }

    #[test]
    fn main_content_rows_preserves_intentional_blank_output_lines() {
        let mut grid = TerminalGrid::new(10, 5, GridLimits::default()).unwrap();

        grid.process(b"before\r\n\r\nafter");

        let rows = grid.main_content_rows();
        let text = rows.iter().map(row_text).collect::<Vec<_>>();
        assert_eq!(text, vec!["before", "", "after"]);
    }

    #[test]
    fn repeated_resize_preserves_live_content_and_cursor() {
        let mut grid = TerminalGrid::new(12, 3, GridLimits::default()).unwrap();
        grid.process(b"abcdefghi");
        for (width, height) in [(5, 3), (12, 4), (4, 5), (12, 3)] {
            grid.resize(width, height).unwrap();
            let rows = grid.main_content_rows();
            assert_eq!(rows.iter().map(row_text).collect::<String>(), "abcdefghi");
            assert_eq!(grid.cursor.col, 9 % usize::from(width));
            assert_eq!(grid.cursor.row, 9 / usize::from(width));
        }
    }

    #[test]
    fn narrowing_resize_retains_lines_moved_to_history() {
        let mut grid = TerminalGrid::new(5, 4, GridLimits::default()).unwrap();
        grid.process(b"one\r\ntwo\r\nthree\r\nfour");
        crate::reflow::reset_projection_stats();
        grid.resize(2, 1).unwrap();
        let stats = crate::reflow::projection_stats();
        assert_eq!(stats.logical_lines_projected, 1);
        assert_eq!(stats.physical_rows_projected, 2);
        let rows = grid.main_content_rows();
        assert_eq!(
            rows.iter().map(row_text).collect::<Vec<_>>(),
            ["on", "e", "tw", "o", "th", "re", "e", "fo", "ur"]
        );
        assert_eq!(
            rows.iter().map(PhysicalRow::wrapped).collect::<Vec<_>>(),
            [true, false, true, false, true, true, false, true, false]
        );
    }

    #[test]
    fn shrinking_height_does_not_project_lines_moved_to_history() {
        let mut grid = TerminalGrid::new(5, 4, GridLimits::default()).unwrap();
        grid.process(b"one\r\ntwo\r\nthree\r\nfour");
        let before = grid.main_content_rows();
        crate::reflow::reset_projection_stats();
        grid.resize(5, 1).unwrap();
        let stats = crate::reflow::projection_stats();
        assert_eq!(stats.logical_lines_projected, 1);
        assert_eq!(stats.physical_rows_projected, 1);
        assert_eq!(grid.main_content_rows(), before);
    }

    #[test]
    fn display_window_prepends_padding_without_reordering_content() {
        let mut grid = TerminalGrid::new(5, 3, GridLimits::default()).unwrap();
        grid.process(b"one\r\ntwo\r\nthree");
        for mode in [GridMode::Main, GridMode::Alternate] {
            grid.set_mode(mode);
            if mode == GridMode::Alternate {
                grid.process(b"alt\r\ntwo\r\nlast");
            }
            let full = grid.display_rows_unpadded(0, 3);
            for offset in 0..=4 {
                let window = grid.display_window(GridRowWindow {
                    scrollback_offset: offset,
                    rows: 5,
                });
                let retained = 3_usize.saturating_sub(offset);
                assert_eq!(window.rows.len(), 5);
                assert_eq!(&window.rows[5 - retained..], &full[..retained]);
                assert!(
                    window.rows[..5 - retained]
                        .iter()
                        .all(|row| row.cells().is_empty())
                );
                assert!(!window.has_more_above);
            }
        }
    }

    #[test]
    fn main_row_windows_match_retained_slices() {
        let mut grid = TerminalGrid::new(3, 3, GridLimits { scrollback_rows: 4 }).unwrap();
        grid.process("\x1b[31m界e\u{301}\r\n\x1b[44m界界\r\nabc界def界ghi界jkl".as_bytes());
        for width in [3, 2, 7] {
            grid.resize(width, 3).unwrap();
            // Assemble the reference without the window collector under test.
            let mut all = Vec::new();
            for line in &grid.main_history {
                all.extend(crate::reflow::project_logical_line(&line.cells, grid.width));
            }
            if !grid.pending_history_cells.is_empty() {
                let mut pending =
                    crate::reflow::project_logical_line(&grid.pending_history_cells, grid.width);
                for row in &mut pending {
                    row.set_wrapped(true);
                }
                all.extend(pending);
            }
            all.extend(grid.main_rows.iter().cloned());
            for offset in (0..=all.len() + 1).chain([usize::MAX]) {
                for requested in [0, 1, 2, 3, 5, usize::MAX] {
                    let end = all.len().saturating_sub(offset);
                    let start = end.saturating_sub(requested);
                    crate::reflow::reset_projection_stats();
                    assert_eq!(
                        grid.main_display_rows(offset, requested),
                        all[start..end],
                        "width {width}, offset {offset}, requested {requested}"
                    );
                    let stats = crate::reflow::projection_stats();
                    assert!(
                        stats.physical_rows_projected <= end - start,
                        "projection exceeded returned window at offset {offset}"
                    );
                    if requested == 0 {
                        assert_eq!(stats.logical_lines_projected, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn projected_history_count_cache_tracks_resize_append_and_eviction() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits { scrollback_rows: 2 }).unwrap();
        grid.process(b"abcdefghij\r\nk\r\nl");
        assert_eq!(grid.history_projected_row_count(), 2);
        grid.resize(3, 2).unwrap();
        assert!(grid.main_history_projected_rows.get().is_none());
        for chunk in [b"m\r\n".as_slice(), b"nopqrst\r\n", b"u\r\n", b"v\r\n"] {
            grid.process(chunk);
            let expected: usize = grid
                .main_history
                .iter()
                .map(|line| line.projected_row_count(grid.width))
                .sum();
            assert_eq!(grid.history_projected_row_count(), expected);
            assert_eq!(grid.main_history_projected_rows.get(), Some(&expected));
            assert_eq!(
                grid.scrollback_rows_hint(),
                grid.all_main_rows_slow().len().saturating_sub(2)
            );
        }
        grid.resize(3, 4).unwrap();
        assert!(grid.main_history_projected_rows.get().is_some());
        grid.process(b"\x1b[3J");
        assert_eq!(grid.history_projected_row_count(), 0);
    }

    #[test]
    fn snapshot_long_history_projects_only_requested_window() {
        let mut grid = TerminalGrid::new(
            10,
            2,
            GridLimits {
                scrollback_rows: 20_000,
            },
        )
        .unwrap();
        let mut original_text = String::new();
        for index in 0..10_000 {
            let row = format!("{index:010}");
            original_text.push_str(&row);
            grid.process(row.as_bytes());
        }
        for completed in [false, true] {
            if completed {
                grid.process(b"\r\nend\r\nlast");
            }
            for offset in [10, 5_000] {
                crate::reflow::reset_projection_stats();
                let snapshot = grid.snapshot(offset, 2);
                assert_eq!(snapshot.rows.len(), 2);
                assert!(snapshot.rows.iter().all(|row| row.wrapped));
                for (index, row) in snapshot.rows.iter().enumerate() {
                    let text: String = row.runs.iter().map(|run| run.text.as_str()).collect();
                    let expected = 10_000 + usize::from(completed) * 2 - offset - 2 + index;
                    assert_eq!(
                        text,
                        format!("{expected:010}"),
                        "completed={completed}, offset={offset}"
                    );
                }
                assert_eq!(crate::reflow::projection_stats().physical_rows_projected, 2);
            }
            let mut resized = grid.clone();
            for width in [5, 20, 10] {
                resized.resize(width, 2).unwrap();
                let full = resized.snapshot(0, 20_010);
                let projected_text: String = full
                    .rows
                    .iter()
                    .flat_map(|row| &row.runs)
                    .map(|run| run.text.as_str())
                    .collect();
                let expected_text = if completed {
                    format!("{original_text}endlast")
                } else {
                    original_text.clone()
                };
                assert_eq!(
                    projected_text, expected_text,
                    "completed={completed}, width={width}"
                );
                let mut recovered = TerminalGrid::from_snapshot(
                    &full,
                    GridLimits {
                        scrollback_rows: 20_000,
                    },
                )
                .unwrap();
                assert_eq!(recovered.snapshot(0, 20_010), full);
                for offset in [10, 1_000] {
                    crate::reflow::reset_projection_stats();
                    let window = resized.snapshot(offset, 2);
                    let end = full.rows.len() - offset;
                    assert_eq!(
                        window.rows,
                        full.rows[end - 2..end],
                        "completed={completed}, width={width}, offset={offset}"
                    );
                    assert_eq!(crate::reflow::projection_stats().physical_rows_projected, 2);
                    assert_eq!(recovered.snapshot(offset, 2).rows, window.rows);
                }
                let mut continued = resized.clone();
                let mut replica = full.clone();
                for output in [
                    b"more\r\nnext".as_slice(),
                    b"\x1b[?1049htui",
                    b"\x1b[?1049ltail",
                ] {
                    continued.process(output);
                    recovered.process(output);
                    let after = continued.snapshot(0, 20_010);
                    let delta = crate::GridDeltaBatch::between(&replica, &after).unwrap();
                    delta.apply_to_snapshot(&mut replica).unwrap();
                    assert_eq!(replica, after);
                    assert_eq!(recovered.snapshot(0, 20_010), after);
                }
            }
        }
    }

    #[test]
    fn pending_history_windows_preserve_rows_and_wrap_boundaries() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process(b"old\r\nolder\r\nabcdefghijklmnop");
        let full = grid.snapshot(0, 20);
        assert!(full.rows.len() > 4);
        assert!(full.rows.iter().any(|row| row.wrapped));
        for offset in 0..full.rows.len() {
            for requested in [2, 3, 4] {
                let window = grid.snapshot(offset, requested);
                let end = full.rows.len().saturating_sub(offset);
                let start = end.saturating_sub(requested);
                assert_eq!(
                    window.rows,
                    full.rows[start..end],
                    "offset={offset}, requested={requested}"
                );
            }
        }
    }

    #[test]
    fn scrollback_hint_counts_projected_rows_across_reflow_and_hydration() {
        let limits = GridLimits {
            scrollback_rows: 20,
        };
        let mut grid = TerminalGrid::new(5, 2, limits).unwrap();
        grid.process(b"abcdefghij\r\nk\r\nl");
        assert_eq!(grid.scrollback_rows_hint(), 2);
        for width in [5, 10, 3, 7] {
            grid.resize(width, 2).unwrap();
            let expected = grid.all_main_rows_slow().len().saturating_sub(2);
            assert_eq!(grid.scrollback_rows_hint(), expected);
            let snapshot = grid.snapshot(0, expected.saturating_add(2));
            assert_eq!(usize::try_from(snapshot.scrollback_rows).unwrap(), expected);
            let hydrated = TerminalGrid::from_snapshot(&snapshot, limits).unwrap();
            assert_eq!(hydrated.scrollback_rows_hint(), expected);
            grid.process(b"\x1b[?1049h");
            assert_eq!(grid.scrollback_rows_hint(), 0);
            grid.process(b"\x1b[?1049l");
            assert_eq!(grid.scrollback_rows_hint(), expected);
        }
    }

    #[test]
    fn main_content_rows_reflows_on_resize() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdefghij");

        grid.resize(10, 5).unwrap();

        let rows = grid.main_content_rows();
        let text = rows.iter().map(row_text).collect::<Vec<_>>();
        assert_eq!(text, vec!["abcdefghij"]);
    }

    #[test]
    fn plain_text_soft_wraps_and_reflows() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"abcdefghij");
        assert_eq!(grid.all_main_rows_slow().len(), 2);
        assert!(grid.all_main_rows_slow()[0].wrapped());
        assert!(!grid.all_main_rows_slow()[1].wrapped());

        grid.resize(10, 2).unwrap();
        let rows = grid.all_main_rows_slow();
        let text_row = rows
            .iter()
            .find(|row| row_text(row) == "abcdefghij")
            .expect("reflowed logical line should be retained");
        assert!(!text_row.wrapped());
    }

    #[test]
    fn resize_reflows_line_split_across_history_and_viewport() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"abcdefghijk");

        grid.resize(10, 2).unwrap();

        let rows = grid.display_rows(0, 2);
        assert_eq!(row_text(&rows[0]), "abcdefghij");
        assert_eq!(row_text(&rows[1]), "k");
    }

    #[test]
    fn hard_newline_does_not_join_during_reflow() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"abc\r\ndef");
        grid.resize(10, 2).unwrap();
        let rows = grid.all_main_rows_slow();
        let texts = rows.iter().map(row_text).collect::<Vec<_>>();
        assert!(texts.windows(2).any(|window| window == ["abc", "def"]));
    }

    #[test]
    fn alternate_screen_is_isolated_from_main_scrollback() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"main");
        grid.set_mode(GridMode::Alternate);
        grid.process(b"alt text wraps");
        grid.resize(8, 2).unwrap();
        grid.set_mode(GridMode::Main);
        assert_eq!(row_text(&grid.viewport_rows()[0]), "main");
    }

    #[test]
    fn alternate_screen_resize_keeps_main_screen_writable_after_exit() {
        let mut grid = TerminalGrid::new(
            5,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"top\r\nbot");
        grid.save_cursor();
        grid.set_mode(GridMode::Alternate);
        grid.process(b"alternate");

        grid.resize(9, 5).unwrap();
        grid.set_mode(GridMode::Main);
        grid.restore_cursor();
        grid.process(b"\r\nPROMPT");

        assert_eq!(grid.viewport_rows().len(), 5);
        assert_eq!(row_text(&grid.viewport_rows()[0]), "top");
        assert_eq!(row_text(&grid.viewport_rows()[1]), "bot");
        assert_eq!(row_text(&grid.viewport_rows()[2]), "PROMPT");
    }

    #[test]
    fn alternate_screen_resize_preserves_main_cursor_anchor_across_width_reflow() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdefgh");
        grid.save_cursor();
        grid.set_mode(GridMode::Alternate);

        grid.resize(10, 4).unwrap();
        grid.set_mode(GridMode::Main);
        grid.restore_cursor();
        grid.process(b"X");

        assert_eq!(grid.viewport_rows().len(), 4);
        assert_eq!(row_text(&grid.viewport_rows()[0]), "abcdefghX");
    }

    #[test]
    fn repeated_alternate_screen_resizes_leave_both_screens_usable() {
        let mut grid = TerminalGrid::new(8, 3, GridLimits::default()).unwrap();
        for (width, height) in [(16, 6), (4, 2), (12, 5), (7, 4)] {
            grid.save_cursor();
            grid.set_mode(GridMode::Alternate);
            grid.process(b"alternate");
            grid.resize(width, height).unwrap();
            grid.set_mode(GridMode::Main);
            grid.restore_cursor();
            grid.process(b"x");

            assert_eq!(grid.viewport_rows().len(), usize::from(height));
            assert!(grid.cursor().row < usize::from(height));
            assert!(grid.cursor().col < usize::from(width));
        }
    }

    #[test]
    fn sgr_styles_are_interned() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"\x1b[31mred\x1b[0m plain");
        let rows = grid.all_main_rows_slow();
        let red = rows[0].cells()[0].style();
        let plain = rows[0].cells()[3].style();
        assert_ne!(red, plain);
        assert_eq!(grid.palette().get(red).fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn wide_chars_keep_spacer_cells() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.process("a界b".as_bytes());
        let rows = grid.all_main_rows_slow();
        assert_eq!(rows[0].cells()[1].text(), "界");
        assert!(rows[0].cells()[2].is_wide_continuation());
    }

    #[test]
    fn combining_chars_attach_to_previous_cell() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process("e\u{301}".as_bytes());

        assert_eq!(grid.all_main_rows_slow()[0].cells()[0].text(), "e\u{301}");
        assert_eq!(grid.cursor().col, 1);
    }

    #[test]
    fn scrollback_limit_bounds_retained_main_rows() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits { scrollback_rows: 1 }).unwrap();
        grid.process(b"1\r\n2\r\n3\r\n4");

        assert_eq!(grid.all_main_rows_slow().len(), 3);
    }

    #[test]
    fn resize_remaps_cursor_to_same_logical_column() {
        let mut grid = TerminalGrid::new(
            10,
            2,
            GridLimits {
                scrollback_rows: 20,
            },
        )
        .unwrap();
        grid.process(b"abcdefg");

        grid.resize(4, 2).unwrap();

        assert_eq!(grid.cursor().col, 3);
        let rows = grid.all_main_rows_slow();
        assert_eq!(row_text(&rows[0]), "abcd");
        assert_eq!(row_text(&rows[1]), "efg");
    }

    #[test]
    fn large_scrollback_resize_and_snapshot_project_only_visible_tail() {
        let mut grid = TerminalGrid::new(
            80,
            20,
            GridLimits {
                scrollback_rows: 5_000,
            },
        )
        .unwrap();
        for index in 0..5_000 {
            grid.process(format!("line-{index:04} payload payload payload\r\n").as_bytes());
        }

        crate::reflow::reset_projection_stats();
        grid.resize(24, 20).unwrap();
        let resize_stats = crate::reflow::projection_stats();
        assert!(
            resize_stats.logical_lines_projected <= 24,
            "resize should project only the live tail, got {resize_stats:?}"
        );

        crate::reflow::reset_projection_stats();
        let snapshot = grid.snapshot(0, 20);
        let snapshot_stats = crate::reflow::projection_stats();
        assert_eq!(snapshot.rows.len(), 20);
        assert!(
            snapshot_stats.logical_lines_projected <= 20,
            "bounded snapshot should project only visible rows, got {snapshot_stats:?}"
        );

        let clamped_snapshot = grid.snapshot(0, usize::MAX);
        assert_eq!(clamped_snapshot.rows.len(), grid.height());
    }

    #[test]
    fn large_scrollback_resize_reflows_and_remains_bounded() {
        let mut grid = TerminalGrid::new(
            80,
            20,
            GridLimits {
                scrollback_rows: 500,
            },
        )
        .unwrap();
        for index in 0..1_500 {
            grid.process(format!("line-{index:04} payload payload payload\r\n").as_bytes());
        }

        grid.resize(24, 20).unwrap();
        assert!(grid.main_history.len() <= 500);
        let narrow_text = crate::visible_text(&grid, 0, 80);
        assert!(narrow_text.contains("line-1499"));
        assert!(narrow_text.contains("payload"));

        grid.resize(100, 20).unwrap();
        assert!(grid.main_history.len() <= 500);
        let wide_text = crate::visible_text(&grid, 0, 40);
        assert!(wide_text.contains("line-1499 payload payload payload"));
    }

    fn row_text(row: &PhysicalRow) -> String {
        row.cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(Cell::text)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}
