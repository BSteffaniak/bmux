use crate::reflow::project_logical_line;
use crate::snapshot::{GridSnapshot, RowSnapshot};
use crate::style::{Color, Style, StyleId, StylePalette};
use serde::{Deserialize, Serialize};
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
    main_history: VecDeque<LogicalLine>,
    main_history_projected_rows: usize,
    pending_history_cells: Vec<Cell>,
    main_rows: VecDeque<PhysicalRow>,
    alt_rows: Vec<PhysicalRow>,
    mode: GridMode,
    cursor: Cursor,
    saved_cursor: Cursor,
    current_style: Style,
    palette: StylePalette,
    revision: u64,
    total_scrolled_rows: u64,
    autowrap: bool,
    pending_wrap: bool,
    scroll_region: Option<(usize, usize)>,
    protocol: ProtocolState,
}

#[derive(Debug, Clone, Default)]
struct LogicalLine {
    cells: Vec<Cell>,
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
            main_history_projected_rows: 0,
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
            current_style: Style::default(),
            palette: StylePalette::default(),
            revision: 0,
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
        let rows = snapshot
            .rows
            .iter()
            .map(|row| row_from_snapshot(row, width))
            .collect::<Vec<_>>();
        let mut main_history = VecDeque::new();
        let mut pending_history_cells = Vec::new();
        let mut main_rows = VecDeque::new();
        let mut alt_rows = vec![PhysicalRow::new(); height];
        match mode {
            GridMode::Main => {
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
        let main_history_projected_rows = main_history
            .iter()
            .map(|line| projected_row_count(&line.cells, width))
            .sum();
        let mut grid = Self {
            width,
            height,
            limits,
            main_history,
            main_history_projected_rows,
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
            current_style: snapshot.current_style,
            palette,
            revision: snapshot.revision,
            total_scrolled_rows: u64::from(snapshot.scrollback_rows),
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
        if self.mode == GridMode::Main {
            self.resize_main_viewport(width, height);
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
                    self.main_history.clear();
                    self.main_history_projected_rows = 0;
                    self.pending_history_cells.clear();
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
            GridMode::Main => self.main_rows.iter().cloned().collect(),
            GridMode::Alternate => self.alt_rows.clone(),
        }
    }

    #[must_use]
    pub fn display_rows(&self, scrollback_offset: usize, rows: usize) -> Vec<PhysicalRow> {
        let requested_rows = rows.max(self.height);
        let mut display_rows = match self.mode {
            GridMode::Main => self.main_display_rows(scrollback_offset, requested_rows),
            GridMode::Alternate => self.alt_display_rows(scrollback_offset, requested_rows),
        };
        if display_rows.len() < requested_rows {
            let missing = requested_rows.saturating_sub(display_rows.len());
            let mut padded = Vec::with_capacity(requested_rows);
            padded.resize_with(missing, PhysicalRow::new);
            padded.extend(display_rows);
            display_rows = padded;
        }
        display_rows
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

    pub(crate) fn main_rows(&self) -> Vec<PhysicalRow> {
        self.all_main_rows()
    }

    #[must_use]
    pub fn all_main_rows(&self) -> Vec<PhysicalRow> {
        self.display_rows(0, self.main_row_count())
    }

    #[must_use]
    pub fn snapshot(&self, scrollback_offset: usize, rows: usize) -> GridSnapshot {
        GridSnapshot::from_grid(self, scrollback_offset, rows)
    }

    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
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
                if let Some(row) = self.main_rows.pop_front() {
                    self.push_history_row(&row);
                }
                self.main_rows.push_back(PhysicalRow::new());
                self.total_scrolled_rows = self.total_scrolled_rows.saturating_add(1);
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

    fn push_history_row(&mut self, row: &PhysicalRow) {
        self.pending_history_cells
            .extend(row_logical_cells(row, self.width));
        if !row.wrapped() {
            let cells = trim_trailing_blank_cells(std::mem::take(&mut self.pending_history_cells));
            self.push_history_line(LogicalLine { cells });
        }
    }

    fn push_history_line(&mut self, line: LogicalLine) {
        self.main_history_projected_rows = self
            .main_history_projected_rows
            .saturating_add(projected_row_count(&line.cells, self.width));
        self.main_history.push_back(line);
    }

    fn history_projected_row_count(&self) -> usize {
        self.main_history
            .iter()
            .map(|line| projected_row_count(&line.cells, self.width))
            .sum()
    }

    fn main_display_rows(
        &self,
        scrollback_offset: usize,
        requested_rows: usize,
    ) -> Vec<PhysicalRow> {
        let mut skipped = 0_usize;
        let mut selected = Vec::with_capacity(requested_rows);
        for row in self.main_rows.iter().rev() {
            collect_reversed_row(
                row.clone(),
                scrollback_offset,
                requested_rows,
                &mut skipped,
                &mut selected,
            );
            if selected.len() >= requested_rows {
                break;
            }
        }
        if selected.len() < requested_rows && !self.pending_history_cells.is_empty() {
            collect_projected_line_reversed(
                &self.pending_history_cells,
                self.width,
                scrollback_offset,
                requested_rows,
                &mut skipped,
                &mut selected,
            );
        }
        for line in self.main_history.iter().rev() {
            if selected.len() >= requested_rows {
                break;
            }
            collect_projected_line_reversed(
                &line.cells,
                self.width,
                scrollback_offset,
                requested_rows,
                &mut skipped,
                &mut selected,
            );
        }
        selected.reverse();
        selected
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

    fn resize_main_viewport(&mut self, new_width: usize, new_height: usize) {
        let mut source_rows = self.main_rows.iter().cloned().collect::<Vec<_>>();
        while source_rows.len() > 1
            && source_rows
                .last()
                .is_some_and(|row| row_is_blank(row) && !row.wrapped())
            && self.cursor.row < source_rows.len().saturating_sub(1)
        {
            source_rows.pop();
        }
        let anchor = self.live_cursor_anchor(&source_rows);
        let live_lines = self.live_logical_lines(&source_rows);
        let mut projected_by_line = Vec::with_capacity(live_lines.len());
        let mut total_rows = 0_usize;
        for line in &live_lines {
            let rows = project_logical_line(&line.cells, new_width);
            total_rows = total_rows.saturating_add(rows.len());
            projected_by_line.push(rows.into_iter().collect::<Vec<_>>());
        }
        let keep_start = total_rows.saturating_sub(new_height);
        let mut row_index = 0_usize;
        let mut next_pending = Vec::new();
        let mut next_rows = VecDeque::new();
        for (line, rows) in live_lines.iter().zip(&projected_by_line) {
            let line_start = row_index;
            let line_end = line_start.saturating_add(rows.len());
            if line_end <= keep_start {
                self.push_history_line(line.clone());
            } else if line_start < keep_start {
                let hidden = keep_start.saturating_sub(line_start);
                for row in rows.iter().take(hidden) {
                    next_pending.extend(row_logical_cells(row, new_width));
                }
                for row in rows.iter().skip(hidden) {
                    next_rows.push_back(row.clone());
                }
            } else {
                for row in rows {
                    next_rows.push_back(row.clone());
                }
            }
            row_index = line_end;
        }
        self.pending_history_cells = trim_trailing_blank_cells(next_pending);
        while next_rows.len() < new_height {
            next_rows.push_back(PhysicalRow::new());
        }
        while next_rows.len() > new_height {
            if let Some(row) = next_rows.pop_front() {
                self.push_history_row(&row);
            }
        }
        self.main_rows = next_rows;
        self.restore_live_cursor_anchor(
            anchor,
            &projected_by_line,
            keep_start,
            new_width,
            new_height,
        );
    }

    fn live_cursor_anchor(&self, source_rows: &[PhysicalRow]) -> Option<CursorAnchor> {
        if self.mode != GridMode::Main || source_rows.is_empty() {
            return None;
        }
        let mut logical_line = 0_usize;
        let mut run_start = 0_usize;
        let mut prefix_cols = logical_width(&self.pending_history_cells);
        for (index, row) in source_rows.iter().enumerate() {
            if index == self.cursor.row.min(source_rows.len().saturating_sub(1)) {
                return Some(CursorAnchor {
                    logical_line,
                    logical_col: prefix_cols
                        .saturating_add(index.saturating_sub(run_start).saturating_mul(self.width))
                        .saturating_add(self.cursor.col),
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
                lines.push(LogicalLine {
                    cells: trim_trailing_blank_cells(std::mem::take(&mut logical)),
                });
            }
        }
        if !logical.is_empty() {
            lines.push(LogicalLine {
                cells: trim_trailing_blank_cells(logical),
            });
        }
        if lines.is_empty() {
            lines.push(LogicalLine::default());
        }
        lines
    }

    fn restore_live_cursor_anchor(
        &mut self,
        anchor: Option<CursorAnchor>,
        projected_by_line: &[Vec<PhysicalRow>],
        keep_start: usize,
        width: usize,
        height: usize,
    ) {
        let Some(anchor) = anchor else {
            self.clamp_cursor();
            return;
        };
        let mut absolute_row = 0_usize;
        for (line_index, rows) in projected_by_line.iter().enumerate() {
            if line_index == anchor.logical_line {
                absolute_row = absolute_row.saturating_add(anchor.logical_col / width.max(1));
                self.cursor.row = absolute_row
                    .saturating_sub(keep_start)
                    .min(height.saturating_sub(1));
                self.cursor.col = (anchor.logical_col % width.max(1)).min(width.saturating_sub(1));
                self.clamp_cursor();
                return;
            }
            absolute_row = absolute_row.saturating_add(rows.len());
        }
        self.clamp_cursor();
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
        while self.main_history.len() > self.limits.scrollback_rows
            || self
                .main_history_projected_rows
                .saturating_add(projected_pending_row_count(
                    &self.pending_history_cells,
                    self.width,
                ))
                > self.limits.scrollback_rows
        {
            if let Some(line) = self.main_history.pop_front() {
                self.main_history_projected_rows = self
                    .main_history_projected_rows
                    .saturating_sub(projected_row_count(&line.cells, self.width));
            } else {
                self.pending_history_cells.clear();
                break;
            }
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

fn hydrate_logical_history(
    rows: &[PhysicalRow],
    width: usize,
    history: &mut VecDeque<LogicalLine>,
    pending: &mut Vec<Cell>,
) {
    for row in rows {
        pending.extend(row_logical_cells(row, width));
        if !row.wrapped() {
            history.push_back(LogicalLine {
                cells: trim_trailing_blank_cells(std::mem::take(pending)),
            });
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
    while cells
        .last()
        .is_some_and(|cell| cell.text() == " " && !cell.is_wide_continuation())
    {
        cells.pop();
    }
    cells
}

fn projected_row_count(cells: &[Cell], width: usize) -> usize {
    project_logical_line(cells, width).len()
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
    row.cells().iter().all(|cell| cell.text() == " ")
}

fn collect_reversed_row(
    row: PhysicalRow,
    scrollback_offset: usize,
    requested_rows: usize,
    skipped: &mut usize,
    selected: &mut Vec<PhysicalRow>,
) {
    if *skipped < scrollback_offset {
        *skipped = skipped.saturating_add(1);
        return;
    }
    if selected.len() < requested_rows {
        selected.push(row);
    }
}

fn collect_projected_line_reversed(
    cells: &[Cell],
    width: usize,
    scrollback_offset: usize,
    requested_rows: usize,
    skipped: &mut usize,
    selected: &mut Vec<PhysicalRow>,
) {
    let rows = project_logical_line(cells, width);
    for row in rows.into_iter().rev() {
        collect_reversed_row(row, scrollback_offset, requested_rows, skipped, selected);
        if selected.len() >= requested_rows {
            break;
        }
    }
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
