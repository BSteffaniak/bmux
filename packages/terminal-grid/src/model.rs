use crate::snapshot::{GridSnapshot, RowSnapshot};
use crate::style::{Color, Style, StyleId, StylePalette};
use std::collections::VecDeque;
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

/// Terminal grid mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    Main,
    Alternate,
}

/// Grid cursor in viewport-relative coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    pub(crate) fn clear_range(&mut self, start: usize, end: usize) {
        if start >= end || start >= self.cells.len() {
            return;
        }
        let clamped_end = end.min(self.cells.len());
        for cell in &mut self.cells[start..clamped_end] {
            *cell = Cell::blank(StyleId::DEFAULT);
        }
        self.trim_trailing_blanks();
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.cells.truncate(len);
        self.trim_trailing_blanks();
    }

    fn trim_trailing_blanks(&mut self) {
        while self
            .cells
            .last()
            .is_some_and(|cell| cell.text == " " && cell.style == StyleId::DEFAULT)
        {
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
pub struct TerminalGrid {
    width: usize,
    height: usize,
    limits: GridLimits,
    main_rows: VecDeque<PhysicalRow>,
    alt_rows: Vec<PhysicalRow>,
    mode: GridMode,
    cursor: Cursor,
    saved_cursor: Cursor,
    current_style: Style,
    palette: StylePalette,
    revision: u64,
    autowrap: bool,
    pending_wrap: bool,
    scroll_region: Option<(usize, usize)>,
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
            main_rows,
            alt_rows: vec![PhysicalRow::new(); height],
            mode: GridMode::Main,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
            saved_cursor: Cursor::default(),
            current_style: Style::default(),
            palette: StylePalette::default(),
            revision: 0,
            autowrap: true,
            pending_wrap: false,
            scroll_region: None,
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
        let rows = snapshot
            .rows
            .iter()
            .map(|row| row_from_snapshot(row, width))
            .collect::<Vec<_>>();
        let mut main_rows = VecDeque::new();
        let mut alt_rows = vec![PhysicalRow::new(); height];
        match mode {
            GridMode::Main => {
                main_rows.extend(rows);
                while main_rows.len() < height {
                    main_rows.push_front(PhysicalRow::new());
                }
            }
            GridMode::Alternate => {
                for (index, row) in rows.into_iter().take(height).enumerate() {
                    alt_rows[index] = row;
                }
                for _ in 0..height {
                    main_rows.push_back(PhysicalRow::new());
                }
            }
        }
        let mut grid = Self {
            width,
            height,
            limits,
            main_rows,
            alt_rows,
            mode,
            cursor: Cursor {
                row: usize::from(snapshot.cursor.row),
                col: usize::from(snapshot.cursor.col),
                visible: snapshot.cursor.visible,
            },
            saved_cursor: Cursor::default(),
            current_style: snapshot.current_style,
            palette,
            revision: snapshot.revision,
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
    pub const fn mode(&self) -> GridMode {
        self.mode
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
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

    /// Resize the terminal. Main-screen retained rows are reflowed by joining
    /// soft-wrapped row runs and splitting them to the new width.
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
        let cursor_anchor = self.main_cursor_anchor();
        crate::reflow::reflow_main_rows(&mut self.main_rows, self.width, width);
        while self.main_rows.len() > height
            && self
                .main_rows
                .back()
                .is_some_and(|row| row.cells().is_empty() && !row.wrapped())
        {
            self.main_rows.pop_back();
        }
        self.width = width;
        self.height = height;
        self.pending_wrap = false;
        while self.main_rows.len() < height {
            self.main_rows.push_front(PhysicalRow::new());
        }
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
        if self.mode == GridMode::Main {
            self.restore_main_cursor_anchor(cursor_anchor);
        }
        self.cursor.row = self.cursor.row.min(height.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(width.saturating_sub(1));
        self.bump_revision();
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
        self.bump_revision();
    }

    pub(crate) fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor.visible = visible;
        self.bump_revision();
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    pub(crate) fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.clamp_cursor();
        self.bump_revision();
    }

    pub(crate) fn set_autowrap(&mut self, enabled: bool) {
        self.autowrap = enabled;
        self.bump_revision();
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
        let (top, bottom) = self.effective_scroll_region();
        if self.cursor.row == bottom {
            if self.mode == GridMode::Main && top == 0 && bottom + 1 == self.height {
                self.scroll_up_one();
            } else {
                self.scroll_region_up(top, bottom, 1);
            }
        } else if self.cursor.row < self.height.saturating_sub(1) {
            self.cursor.row += 1;
        }
        self.bump_revision();
    }

    pub(crate) fn reverse_index(&mut self) {
        self.pending_wrap = false;
        let (top, bottom) = self.effective_scroll_region();
        if self.cursor.row == top {
            self.scroll_region_down(top, bottom, 1);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(1);
        }
        self.bump_revision();
    }

    pub(crate) fn print_char(&mut self, ch: char) {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if char_width == 0 {
            self.append_combining(ch);
            self.bump_revision();
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
        self.bump_revision();
    }

    pub(crate) fn erase_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.cursor.row + 1..self.height {
                    self.clear_viewport_row(row);
                }
            }
            1 => {
                for row in 0..self.cursor.row {
                    self.clear_viewport_row(row);
                }
                self.erase_line(1);
            }
            2 | 3 => {
                for row in 0..self.height {
                    self.clear_viewport_row(row);
                }
                if mode == 3 && self.mode == GridMode::Main {
                    let viewport = self.viewport_rows().clone();
                    self.main_rows.clear();
                    self.main_rows.extend(viewport);
                }
            }
            _ => {}
        }
        self.bump_revision();
    }

    pub(crate) fn erase_line(&mut self, mode: usize) {
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        match mode {
            0 => self.active_row_mut(row).clear_range(col, width),
            1 => self
                .active_row_mut(row)
                .clear_range(0, col.saturating_add(1)),
            2 => self.active_row_mut(row).clear_range(0, width),
            _ => {}
        }
        self.bump_revision();
    }

    pub(crate) fn erase_chars(&mut self, count: usize) {
        let start = self.cursor.col;
        let end = start.saturating_add(count.max(1)).min(self.width);
        let row = self.cursor_absolute_row();
        self.active_row_mut(row).clear_range(start, end);
        self.bump_revision();
    }

    pub(crate) fn insert_blank_chars(&mut self, count: usize) {
        let count = count.max(1).min(self.width.saturating_sub(self.cursor.col));
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        let mut cells = self.active_row_mut(row).visual_cells(width);
        for index in (col..width).rev() {
            cells[index] = if index >= col.saturating_add(count) {
                cells[index - count].clone()
            } else {
                Cell::blank(StyleId::DEFAULT)
            };
        }
        self.replace_active_row(row, cells);
        self.bump_revision();
    }

    pub(crate) fn delete_chars(&mut self, count: usize) {
        let count = count.max(1).min(self.width.saturating_sub(self.cursor.col));
        let width = self.width;
        let col = self.cursor.col;
        let row = self.cursor_absolute_row();
        let mut cells = self.active_row_mut(row).visual_cells(width);
        for index in col..width {
            cells[index] = if index + count < width {
                cells[index + count].clone()
            } else {
                Cell::blank(StyleId::DEFAULT)
            };
        }
        self.replace_active_row(row, cells);
        self.bump_revision();
    }

    pub(crate) fn insert_blank_lines(&mut self, count: usize) {
        let (_, bottom) = self.effective_scroll_region();
        if self.cursor.row > bottom {
            return;
        }
        self.scroll_region_down(self.cursor.row, bottom, count.max(1));
        self.bump_revision();
    }

    pub(crate) fn delete_lines(&mut self, count: usize) {
        let (_, bottom) = self.effective_scroll_region();
        if self.cursor.row > bottom {
            return;
        }
        self.scroll_region_up(self.cursor.row, bottom, count.max(1));
        self.bump_revision();
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
            GridMode::Main => self
                .main_rows
                .iter()
                .skip(self.main_rows.len().saturating_sub(self.height))
                .cloned()
                .collect(),
            GridMode::Alternate => self.alt_rows.clone(),
        }
    }

    #[must_use]
    pub fn display_rows(&self, scrollback_offset: usize, rows: usize) -> Vec<PhysicalRow> {
        let all_rows = match self.mode {
            GridMode::Main => self.all_main_rows(),
            GridMode::Alternate => self.viewport_rows(),
        };
        let end = all_rows
            .len()
            .saturating_sub(scrollback_offset.min(all_rows.len()));
        let requested_rows = rows.max(self.height);
        let start = end.saturating_sub(requested_rows);
        let mut display_rows = all_rows[start..end].to_vec();
        while display_rows.len() < requested_rows {
            display_rows.insert(0, PhysicalRow::new());
        }
        display_rows
    }

    #[must_use]
    pub fn all_main_rows(&self) -> Vec<PhysicalRow> {
        self.main_rows.iter().cloned().collect()
    }

    #[must_use]
    pub fn snapshot(&self, scrollback_offset: usize, rows: usize) -> GridSnapshot {
        GridSnapshot::from_grid(self, scrollback_offset, rows)
    }

    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn cursor_absolute_row(&self) -> usize {
        match self.mode {
            GridMode::Main => self
                .main_rows
                .len()
                .saturating_sub(self.height)
                .saturating_add(self.cursor.row),
            GridMode::Alternate => self.cursor.row,
        }
    }

    fn main_cursor_anchor(&self) -> Option<CursorAnchor> {
        if self.mode != GridMode::Main || self.main_rows.is_empty() {
            return None;
        }
        let cursor_absolute_row = self.cursor_absolute_row().min(self.main_rows.len() - 1);
        let mut logical_line = 0_usize;
        let mut logical_start = 0_usize;
        for (index, row) in self.main_rows.iter().enumerate() {
            if index == cursor_absolute_row {
                let logical_col = self.logical_col_in_run(logical_start, cursor_absolute_row);
                return Some(CursorAnchor {
                    logical_line,
                    logical_col,
                });
            }
            if !row.wrapped() {
                logical_line = logical_line.saturating_add(1);
                logical_start = index.saturating_add(1);
            }
        }
        None
    }

    fn logical_col_in_run(&self, start_row: usize, cursor_row: usize) -> usize {
        let prefix_rows = cursor_row.saturating_sub(start_row);
        prefix_rows
            .saturating_mul(self.width)
            .saturating_add(self.cursor.col)
    }

    fn restore_main_cursor_anchor(&mut self, anchor: Option<CursorAnchor>) {
        let Some(anchor) = anchor else {
            self.clamp_cursor();
            return;
        };
        if self.main_rows.is_empty() {
            self.cursor.row = 0;
            self.cursor.col = 0;
            return;
        }
        let mut logical_line = 0_usize;
        let mut run_start = 0_usize;
        for (index, row) in self.main_rows.iter().enumerate() {
            if logical_line == anchor.logical_line && !row.wrapped() {
                self.place_cursor_in_main_run(run_start, index, anchor.logical_col);
                return;
            }
            if !row.wrapped() {
                logical_line = logical_line.saturating_add(1);
                run_start = index.saturating_add(1);
            }
        }
        let last_row = self.main_rows.len().saturating_sub(1);
        if logical_line == anchor.logical_line {
            self.place_cursor_in_main_run(run_start, last_row, anchor.logical_col);
        } else {
            self.place_cursor_absolute(last_row, self.width.saturating_sub(1));
        }
    }

    fn place_cursor_in_main_run(&mut self, start_row: usize, end_row: usize, logical_col: usize) {
        let row_offset = if self.width == 0 {
            0
        } else {
            logical_col / self.width
        };
        let absolute_row = start_row.saturating_add(row_offset).min(end_row);
        let col = if self.width == 0 {
            0
        } else {
            logical_col % self.width
        };
        self.place_cursor_absolute(absolute_row, col);
    }

    fn place_cursor_absolute(&mut self, absolute_row: usize, col: usize) {
        let viewport_start = self.main_rows.len().saturating_sub(self.height);
        self.cursor.row = absolute_row.saturating_sub(viewport_start);
        self.cursor.col = col;
        self.clamp_cursor();
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

    fn viewport_absolute_row(&self, row: usize) -> usize {
        match self.mode {
            GridMode::Main => self
                .main_rows
                .len()
                .saturating_sub(self.height)
                .saturating_add(row),
            GridMode::Alternate => row,
        }
    }

    fn viewport_row(&self, row: usize) -> PhysicalRow {
        match self.mode {
            GridMode::Main => self.main_rows[self.viewport_absolute_row(row)].clone(),
            GridMode::Alternate => self.alt_rows[row].clone(),
        }
    }

    fn set_viewport_row(&mut self, row: usize, value: PhysicalRow) {
        let absolute = self.viewport_absolute_row(row);
        *self.active_row_mut(absolute) = value;
    }

    fn clear_viewport_row(&mut self, row: usize) {
        self.set_viewport_row(row, PhysicalRow::new());
    }

    fn effective_scroll_region(&self) -> (usize, usize) {
        self.scroll_region
            .unwrap_or((0, self.height.saturating_sub(1)))
    }

    pub(crate) fn scroll_region_up(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= bottom || bottom >= self.height {
            return;
        }
        for _ in 0..count.min(bottom - top + 1) {
            for row in top..bottom {
                let next = self.viewport_row(row + 1);
                self.set_viewport_row(row, next);
            }
            self.clear_viewport_row(bottom);
        }
    }

    pub(crate) fn scroll_region_down(&mut self, top: usize, bottom: usize, count: usize) {
        if top >= bottom || bottom >= self.height {
            return;
        }
        for _ in 0..count.min(bottom - top + 1) {
            for row in (top + 1..=bottom).rev() {
                let previous = self.viewport_row(row - 1);
                self.set_viewport_row(row, previous);
            }
            self.clear_viewport_row(top);
        }
    }

    fn scroll_up_one(&mut self) {
        match self.mode {
            GridMode::Main => {
                self.main_rows.push_back(PhysicalRow::new());
                self.evict_excess_history();
            }
            GridMode::Alternate => {
                if !self.alt_rows.is_empty() {
                    self.alt_rows.remove(0);
                    self.alt_rows.push(PhysicalRow::new());
                }
            }
        }
    }

    fn mark_current_row_wrapped(&mut self) {
        let row = self.cursor_absolute_row();
        self.active_row_mut(row).set_wrapped(true);
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

    fn evict_excess_history(&mut self) {
        let max_rows = self.limits.scrollback_rows.saturating_add(self.height);
        while self.main_rows.len() > max_rows.max(self.height) {
            self.main_rows.pop_front();
        }
    }
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
        assert_eq!(grid.all_main_rows().len(), 2);
        assert!(grid.all_main_rows()[0].wrapped());
        assert!(!grid.all_main_rows()[1].wrapped());

        grid.resize(10, 2).unwrap();
        let rows = grid.all_main_rows();
        let text_row = rows
            .iter()
            .find(|row| row_text(row) == "abcdefghij")
            .expect("reflowed logical line should be retained");
        assert!(!text_row.wrapped());
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
        let rows = grid.all_main_rows();
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
    fn sgr_styles_are_interned() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"\x1b[31mred\x1b[0m plain");
        let rows = grid.all_main_rows();
        let red = rows[0].cells()[0].style();
        let plain = rows[0].cells()[3].style();
        assert_ne!(red, plain);
        assert_eq!(grid.palette().get(red).fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn wide_chars_keep_spacer_cells() {
        let mut grid = TerminalGrid::new(4, 2, GridLimits::default()).unwrap();
        grid.process("a界b".as_bytes());
        let rows = grid.all_main_rows();
        assert_eq!(rows[0].cells()[1].text(), "界");
        assert!(rows[0].cells()[2].is_wide_continuation());
    }

    #[test]
    fn combining_chars_attach_to_previous_cell() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process("e\u{301}".as_bytes());

        assert_eq!(grid.all_main_rows()[0].cells()[0].text(), "e\u{301}");
        assert_eq!(grid.cursor().col, 1);
    }

    #[test]
    fn scrollback_limit_bounds_retained_main_rows() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits { scrollback_rows: 1 }).unwrap();
        grid.process(b"1\r\n2\r\n3\r\n4");

        assert_eq!(grid.all_main_rows().len(), 3);
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
        let rows = grid.all_main_rows();
        assert_eq!(row_text(&rows[0]), "abcd");
        assert_eq!(row_text(&rows[1]), "efg");
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
