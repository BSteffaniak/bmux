//! Frames: how a grid's state reaches a replica of it without its output being parsed again.
//!
//! A grid records damage as it writes: each row carries the epoch it last changed in, scrolls
//! are logged as moves of rows within a screen, and lines entering history are counted. A frame
//! for a replica at some [`ReplicaCursor`] carries what changed since: the moves, then every row
//! changed since (at its final position, with its final content), the history lines pushed
//! since, the palette entries the replica lacks, and the terminal state. A replica applies it
//! by moving and overwriting rows, never by parsing, so its cost follows screen size and frame
//! rate, not how much a program wrote: intermediate states are never sent.
//!
//! A replica that cannot be brought forward exactly (it is new, its epoch fell out of the move
//! log, or the grid was reset or resized) is sent a reset frame, which replaces its whole state.

use super::{
    Cell, CellText, Cursor, GridMode, LogicalLine, PhysicalRow, TerminalGrid, ViewportScroll,
    history_cells_bytes,
};
use crate::snapshot::{CursorSnapshot, ScrollRegionSnapshot, TerminalStateSnapshot};
use crate::style::{Style, StyleId, StylePalette};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::OnceLock;

/// Moves kept for replicas behind the grid; one further behind is sent a reset frame.
const MAX_LOGGED_MOVES: usize = 4096;

/// Where a replica is: the last frame it applied. A new replica is at the default, which is
/// behind everything, so its first frame is a reset frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaCursor {
    /// The damage epoch the replica's rows are current to.
    pub epoch: u64,
    /// History lines pushed by the grid that the replica has been sent (or skipped).
    pub history: u64,
    /// Palette entries the replica holds.
    pub styles: u32,
}

/// Which screen a row or move belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameScreen {
    Main,
    Alternate,
}

/// Rows `top..=bottom` of a screen moved by `rows`, positive toward `top`. Rows moved out of
/// the region are dropped (history arrives as lines); rows exposed are always sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMove {
    pub screen: FrameScreen,
    pub top: u16,
    pub bottom: u16,
    pub rows: i32,
}

/// One row as it now is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRow {
    pub screen: FrameScreen,
    pub index: u32,
    pub wrapped: bool,
    pub cells: FrameLine,
}

/// A row's or line's cells: their text joined, with style runs and wide cells, or each cell as
/// it is when the compact form cannot hold them exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameLine {
    Compact {
        text: String,
        /// Where each cell's text ends, when some cell is not exactly one character.
        ends: Vec<u32>,
        /// Cells two columns wide; each is followed by its continuation, which is not counted.
        wide: Vec<u32>,
        /// The first cell's style id.
        style: u32,
        /// Each later style run's first cell and style id.
        runs: Vec<(u32, u32)>,
        /// Cells, continuations not counted.
        len: u32,
    },
    Cells(Vec<FrameCell>),
}

/// A cell, as [`FrameLine::Cells`] carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCell {
    pub text: String,
    pub style: u32,
    pub width: u8,
    pub continuation: bool,
}

/// Autowrap and the pending wrap, live and saved with the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameWrap {
    pub autowrap: bool,
    pub pending: bool,
    pub saved_pending: bool,
}

/// The grid's terminal state, sent whole in every frame (it is small).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameState {
    pub screen: FrameScreen,
    pub cursor: CursorSnapshot,
    pub saved_cursor: CursorSnapshot,
    pub wrap: FrameWrap,
    pub characters: crate::CharacterState,
    pub saved_characters: crate::CharacterState,
    pub current_style: Style,
    pub terminal_state: TerminalStateSnapshot,
    pub scroll_region: Option<ScrollRegionSnapshot>,
    pub protocol: crate::ProtocolState,
    pub total_scrolled_rows: u64,
    pub history_truncated: bool,
}

/// What a replica applies to come level with the grid: see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridFrame {
    /// The cursor the replica must be at; ignored by a reset frame.
    pub base: ReplicaCursor,
    /// The replica's cursor once applied.
    pub next: ReplicaCursor,
    /// Replace the replica's whole state: palette, rows, history.
    pub reset: bool,
    pub width: u16,
    pub height: u16,
    /// Palette entries from `base.styles` (from 0 in a reset frame).
    pub styles: Vec<Style>,
    pub moves: Vec<FrameMove>,
    pub rows: Vec<FrameRow>,
    /// Clear history before appending `history`.
    pub history_cleared: bool,
    /// History lines pushed since `base`, oldest first.
    pub history: Vec<FrameLine>,
    /// History lines the grid retains: the replica drops its oldest beyond these.
    pub history_retained: u32,
    /// The open history line (the start of a wrapped line scrolled off), when it changed.
    pub pending_history: Option<FrameLine>,
    pub state: FrameState,
}

impl GridFrame {
    /// Whether the frame changes nothing but state (no rows, moves, history or styles).
    #[must_use]
    pub fn is_state_only(&self) -> bool {
        !self.reset
            && self.styles.is_empty()
            && self.moves.is_empty()
            && self.rows.is_empty()
            && !self.history_cleared
            && self.history.is_empty()
            && self.pending_history.is_none()
    }
}

/// Why a frame could not be applied: the replica must be sent a reset frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameApplyError {
    #[error("frame base does not match the replica")]
    BaseMismatch,
    #[error("frame is malformed: {0}")]
    Malformed(&'static str),
}

/// What a grid records for its replicas (see the module documentation).
#[derive(Debug, Clone, Default)]
pub(super) struct Damage {
    /// The last sealed epoch: changes since belong to `epoch + 1`.
    epoch: u64,
    /// Moves in the epochs they happened in, oldest first.
    moves: VecDeque<(u64, FrameMove)>,
    /// The first epoch whose moves are all still logged.
    moves_from: u64,
    /// The last epoch the grid was reset or resized in: replicas before it get a reset frame.
    reset_at: u64,
    /// Lines pushed into history, ever.
    history_pushed: u64,
    /// The last epoch history was cleared in, and `history_pushed` then.
    history_cleared: Option<(u64, u64)>,
    /// The last epoch the open history line changed in.
    pending_at: u64,
}

impl Damage {
    const fn open(&self) -> u64 {
        self.epoch + 1
    }
}

impl PhysicalRow {
    /// Seal this row's change, if it has one, into `epoch`.
    fn seal(&mut self, epoch: u64) {
        if std::mem::take(&mut self.fresh) {
            self.stamp = epoch;
        }
    }
}

impl TerminalGrid {
    /// Note that rows `top..=bottom` of the active screen moved by `rows` (positive up), for
    /// replicas.
    pub(super) fn log_move(&mut self, top: usize, bottom: usize, rows: i64) {
        let screen = match self.mode {
            GridMode::Main => FrameScreen::Main,
            GridMode::Alternate => FrameScreen::Alternate,
        };
        let (Ok(top), Ok(bottom), Ok(rows)) = (
            u16::try_from(top),
            u16::try_from(bottom),
            i32::try_from(rows),
        ) else {
            self.log_reset();
            return;
        };
        let open = self.damage.open();
        // Scrolling a region again the same way within an epoch is one longer scroll: rows moved
        // out are gone either way, and every row exposed is sent.
        if let Some((epoch, last)) = self.damage.moves.back_mut()
            && *epoch == open
            && (last.screen, last.top, last.bottom) == (screen, top, bottom)
            && (last.rows > 0) == (rows > 0)
        {
            last.rows = last.rows.saturating_add(rows);
            return;
        }
        self.damage.moves.push_back((
            open,
            FrameMove {
                screen,
                top,
                bottom,
                rows,
            },
        ));
        while self.damage.moves.len() > MAX_LOGGED_MOVES {
            if let Some((epoch, _)) = self.damage.moves.pop_front() {
                self.damage.moves_from = epoch + 1;
            }
        }
    }

    /// Note a change replicas cannot follow row by row (a resize, a reset): each is sent a reset
    /// frame next.
    pub(super) fn log_reset(&mut self) {
        self.damage.reset_at = self.damage.open();
        self.damage.moves.clear();
        self.damage.moves_from = self.damage.open();
    }

    /// Note that a line entered history.
    pub(super) const fn log_history_push(&mut self) {
        self.damage.history_pushed += 1;
    }

    /// Note that history was cleared.
    pub(super) const fn log_history_clear(&mut self) {
        self.damage.history_cleared = Some((self.damage.open(), self.damage.history_pushed));
        self.log_pending_change();
    }

    /// Note that the open history line changed.
    pub(super) const fn log_pending_change(&mut self) {
        self.damage.pending_at = self.damage.open();
    }

    /// Replica bookkeeping to carry across a reset that rebuilds the grid, noting the reset.
    pub(super) fn take_damage_for_reset(&mut self) -> Damage {
        let mut damage = std::mem::take(&mut self.damage);
        damage.reset_at = damage.open();
        damage.moves.clear();
        damage.moves_from = damage.open();
        damage.history_cleared = Some((damage.open(), damage.history_pushed));
        damage
    }

    pub(super) fn restore_damage(&mut self, damage: Damage) {
        self.damage = damage;
    }

    /// The frame that brings a replica at `base` level with this grid, sealing the changes so
    /// far into an epoch. At most `max_history_lines` history lines are sent; older ones the
    /// replica lacks are skipped, and it marks its history truncated.
    pub fn frame_since(&mut self, base: ReplicaCursor, max_history_lines: usize) -> GridFrame {
        let sealed = self.damage.open();
        for row in self.main_rows.iter_mut().chain(self.alt_rows.iter_mut()) {
            row.seal(sealed);
        }
        self.damage.epoch = sealed;
        let reset = base.epoch == 0
            || base.epoch > sealed
            || base.epoch < self.damage.reset_at
            || base.epoch + 1 < self.damage.moves_from
            || base.styles as usize > self.palette.len();
        let since = if reset { 0 } else { base.epoch };
        let moves = if reset {
            Vec::new()
        } else {
            self.damage
                .moves
                .iter()
                .filter(|(epoch, _)| *epoch > since)
                .map(|(_, movement)| *movement)
                .collect()
        };
        let rows = self.frame_rows(since);
        let styles_from = if reset { 0 } else { base.styles as usize };
        let styles = self.palette.styles()[styles_from..].to_vec();
        let (history_cleared, history_from) = match self.damage.history_cleared {
            _ if reset => (true, 0),
            Some((epoch, pushed)) if epoch > since => (true, pushed.max(base.history)),
            _ => (false, base.history),
        };
        let unsent = usize::try_from(self.damage.history_pushed.saturating_sub(history_from))
            .unwrap_or(usize::MAX);
        let send = unsent.min(self.main_history.len()).min(max_history_lines);
        let history = self
            .main_history
            .iter()
            .skip(self.main_history.len() - send)
            .map(|line| FrameLine::encode(&line.cells()))
            .collect();
        let pending_history = (reset || self.damage.pending_at > since)
            .then(|| FrameLine::encode(&self.pending_history_cells));
        GridFrame {
            base,
            next: ReplicaCursor {
                epoch: sealed,
                history: self.damage.history_pushed,
                styles: u32::try_from(self.palette.len()).unwrap_or(u32::MAX),
            },
            reset,
            width: u16::try_from(self.width).unwrap_or(u16::MAX),
            height: u16::try_from(self.height).unwrap_or(u16::MAX),
            styles,
            moves,
            rows,
            history_cleared,
            history,
            history_retained: u32::try_from(self.main_history.len()).unwrap_or(u32::MAX),
            pending_history,
            state: self.frame_state(),
        }
    }

    fn frame_rows(&self, since: u64) -> Vec<FrameRow> {
        let main = self.main_rows.iter().map(|row| (FrameScreen::Main, row));
        let alternate = self
            .alt_rows
            .iter()
            .map(|row| (FrameScreen::Alternate, row));
        let mut rows = Vec::new();
        let mut index = [0_u32; 2];
        for (screen, row) in main.chain(alternate) {
            let slot = &mut index[usize::from(screen == FrameScreen::Alternate)];
            if row.stamp > since {
                rows.push(FrameRow {
                    screen,
                    index: *slot,
                    wrapped: row.wrapped,
                    cells: FrameLine::encode(&row.cells),
                });
            }
            *slot += 1;
        }
        rows
    }

    fn frame_state(&self) -> FrameState {
        let cursor = |cursor: Cursor| CursorSnapshot {
            row: u16::try_from(cursor.row).unwrap_or(u16::MAX),
            col: u16::try_from(cursor.col).unwrap_or(u16::MAX),
            visible: cursor.visible,
        };
        FrameState {
            screen: match self.mode {
                GridMode::Main => FrameScreen::Main,
                GridMode::Alternate => FrameScreen::Alternate,
            },
            cursor: cursor(self.cursor),
            saved_cursor: cursor(self.saved_cursor),
            wrap: FrameWrap {
                autowrap: self.autowrap,
                pending: self.pending_wrap,
                saved_pending: self.saved_pending_wrap,
            },
            characters: self.characters,
            saved_characters: self.saved_characters,
            current_style: self.current_style,
            terminal_state: self.terminal_state.clone(),
            scroll_region: self
                .scroll_region
                .map(|(top, bottom)| ScrollRegionSnapshot {
                    top: u16::try_from(top).unwrap_or(u16::MAX),
                    bottom: u16::try_from(bottom).unwrap_or(u16::MAX),
                }),
            protocol: self.protocol,
            total_scrolled_rows: self.total_scrolled_rows,
            history_truncated: self.history_truncated,
        }
    }

    /// Apply a frame built by [`Self::frame_since`] for this replica's `cursor`, and advance it.
    /// A replica that sees an error must be sent a reset frame.
    ///
    /// # Errors
    /// Returns an error when the frame is not for `cursor` or is malformed.
    pub fn apply_frame(
        &mut self,
        frame: &GridFrame,
        cursor: &mut ReplicaCursor,
    ) -> Result<(), FrameApplyError> {
        if !frame.reset && frame.base != *cursor {
            return Err(FrameApplyError::BaseMismatch);
        }
        let (width, height) = (usize::from(frame.width), usize::from(frame.height));
        if width == 0 || height == 0 {
            return Err(FrameApplyError::Malformed("zero dimensions"));
        }
        if frame.reset {
            self.palette = StylePalette::from_styles(frame.styles.clone());
            self.width = width;
            self.height = height;
            self.main_rows = std::iter::repeat_with(PhysicalRow::new)
                .take(height)
                .collect();
            self.alt_rows = vec![PhysicalRow::new(); height];
            self.viewport_scroll = ViewportScroll::Unknown;
        } else {
            if (width, height) != (self.width, self.height) {
                return Err(FrameApplyError::Malformed("size changed without a reset"));
            }
            for style in &frame.styles {
                self.palette.push(*style);
            }
        }
        let styles = self.palette.len();
        for movement in &frame.moves {
            self.apply_move(*movement)?;
        }
        for row in &frame.rows {
            let index = row.index as usize;
            if index >= height {
                return Err(FrameApplyError::Malformed("row outside the screen"));
            }
            let cells = row.cells.decode(styles)?;
            let target = match row.screen {
                FrameScreen::Main => &mut self.main_rows[index],
                FrameScreen::Alternate => &mut self.alt_rows[index],
            };
            target.cells = cells;
            target.wrapped = row.wrapped;
            target.changed = true;
            target.fresh = true;
        }
        self.apply_history(frame, styles)?;
        self.apply_state(&frame.state)?;
        *cursor = frame.next;
        Ok(())
    }

    fn apply_move(&mut self, movement: FrameMove) -> Result<(), FrameApplyError> {
        let (top, bottom) = (usize::from(movement.top), usize::from(movement.bottom));
        if top > bottom || bottom >= self.height {
            return Err(FrameApplyError::Malformed("move outside the screen"));
        }
        let span = bottom - top + 1;
        let shift = usize::try_from(movement.rows.unsigned_abs())
            .unwrap_or(usize::MAX)
            .min(span);
        let up = movement.rows > 0;
        match movement.screen {
            FrameScreen::Main => {
                let (front, back) = self.main_rows.as_mut_slices();
                if back.is_empty() {
                    rotate_region(&mut front[top..=bottom], shift, up);
                } else {
                    self.main_rows.make_contiguous();
                    rotate_region(
                        &mut self.main_rows.as_mut_slices().0[top..=bottom],
                        shift,
                        up,
                    );
                }
            }
            FrameScreen::Alternate => rotate_region(&mut self.alt_rows[top..=bottom], shift, up),
        }
        let active = match movement.screen {
            FrameScreen::Main => GridMode::Main,
            FrameScreen::Alternate => GridMode::Alternate,
        };
        if active == self.mode {
            self.note_viewport_scroll(top, bottom, i64::from(movement.rows));
        }
        Ok(())
    }

    fn apply_history(&mut self, frame: &GridFrame, styles: usize) -> Result<(), FrameApplyError> {
        if frame.history_cleared {
            let retained: usize = self
                .main_history
                .iter()
                .map(LogicalLine::history_bytes)
                .sum();
            self.history_bytes = self.history_bytes.saturating_sub(retained);
            self.main_history.clear();
            self.main_history_projected_rows = OnceLock::from(0);
        }
        // Lines the grid pushed that this frame does not carry leave a gap in the replica's
        // history; a reset frame starts it over, marked only by what it skips.
        let skipped = if frame.reset {
            frame.history.len() < frame.history_retained as usize
        } else {
            frame.next.history.saturating_sub(frame.base.history) > frame.history.len() as u64
        };
        if frame.reset {
            self.history_truncated = false;
        }
        self.history_truncated |= skipped;
        for line in &frame.history {
            let cells = line.decode(styles)?;
            self.push_history_line(LogicalLine::new(cells));
        }
        while self.main_history.len() > frame.history_retained as usize {
            let Some(line) = self.main_history.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(line.history_bytes());
            if let Some(count) = self.main_history_projected_rows.get_mut() {
                *count = count.saturating_sub(line.projected_row_count(self.width));
            }
        }
        if let Some(pending) = &frame.pending_history {
            let cells = pending.decode(styles)?;
            self.history_bytes = self
                .history_bytes
                .saturating_sub(history_cells_bytes(&self.pending_history_cells))
                .saturating_add(history_cells_bytes(&cells));
            self.pending_history_cells = cells;
        }
        Ok(())
    }

    fn apply_state(&mut self, state: &FrameState) -> Result<(), FrameApplyError> {
        let cursor = |cursor: CursorSnapshot| Cursor {
            row: usize::from(cursor.row),
            col: usize::from(cursor.col),
            visible: cursor.visible,
        };
        if usize::from(state.cursor.row) >= self.height
            || usize::from(state.cursor.col) >= self.width
        {
            return Err(FrameApplyError::Malformed("cursor outside the screen"));
        }
        let mode = match state.screen {
            FrameScreen::Main => GridMode::Main,
            FrameScreen::Alternate => GridMode::Alternate,
        };
        if mode != self.mode {
            // The other screen's rows replace these; a renderer redraws.
            self.viewport_scroll = ViewportScroll::Unknown;
            self.mode = mode;
        }
        self.cursor = cursor(state.cursor);
        self.saved_cursor = cursor(state.saved_cursor);
        self.autowrap = state.wrap.autowrap;
        self.pending_wrap = state.wrap.pending;
        self.saved_pending_wrap = state.wrap.saved_pending;
        self.characters = state.characters;
        self.saved_characters = state.saved_characters;
        self.current_style = state.current_style;
        self.terminal_state = state.terminal_state.clone();
        self.scroll_region = state
            .scroll_region
            .map(|region| (usize::from(region.top), usize::from(region.bottom)));
        self.protocol = state.protocol;
        self.total_scrolled_rows = state.total_scrolled_rows;
        self.history_truncated |= state.history_truncated;
        self.bump_content_revision();
        Ok(())
    }
}

/// Move `rows` by `shift` toward the start (`up`) or the end, filling the rows exposed at the
/// other end with blank ones.
fn rotate_region(rows: &mut [PhysicalRow], shift: usize, up: bool) {
    let len = rows.len();
    if up {
        rows.rotate_left(shift);
        for row in &mut rows[len - shift..] {
            *row = PhysicalRow::new();
        }
    } else {
        rows.rotate_right(shift);
        for row in &mut rows[..shift] {
            *row = PhysicalRow::new();
        }
    }
}

impl FrameLine {
    /// `cells` as a frame carries them.
    pub(super) fn encode(cells: &[Cell]) -> Self {
        Self::encode_compact(cells).unwrap_or_else(|| {
            Self::Cells(
                cells
                    .iter()
                    .map(|cell| FrameCell {
                        text: cell.text().to_owned(),
                        style: cell.style.0,
                        width: cell.width,
                        continuation: cell.wide_continuation,
                    })
                    .collect(),
            )
        })
    }

    /// The compact form, when it holds `cells` exactly: every cell one or two columns wide, each
    /// two-column cell followed by a continuation of its style, and no other continuation.
    fn encode_compact(cells: &[Cell]) -> Option<Self> {
        let style = cells.first().map_or(0, |cell| cell.style.0);
        let (mut text, mut ends, mut wide, mut runs) =
            (String::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut current, mut single, mut index) = (style, true, 0_u32);
        let mut iter = cells.iter();
        while let Some(cell) = iter.next() {
            if cell.wide_continuation || !(1..=2).contains(&cell.width) {
                return None;
            }
            if cell.width == 2 {
                let continuation = iter.next()?;
                if !continuation.wide_continuation || continuation.style != cell.style {
                    return None;
                }
                wide.push(index);
            }
            let cell_text = cell.text();
            single &= cell_text.chars().count() == 1;
            text.push_str(cell_text);
            ends.push(u32::try_from(text.len()).ok()?);
            if cell.style.0 != current {
                runs.push((index, cell.style.0));
                current = cell.style.0;
            }
            index += 1;
        }
        if single {
            ends.clear();
        }
        Some(Self::Compact {
            text,
            ends,
            wide,
            style,
            runs,
            len: index,
        })
    }

    /// The cells, their styles checked against a palette of `styles` entries.
    pub(super) fn decode(&self, styles: usize) -> Result<Vec<Cell>, FrameApplyError> {
        let style_id = |id: u32| {
            if (id as usize) < styles.max(1) {
                Ok(StyleId(id))
            } else {
                Err(FrameApplyError::Malformed("style outside the palette"))
            }
        };
        match self {
            Self::Cells(cells) => cells
                .iter()
                .map(|cell| {
                    let style = style_id(cell.style)?;
                    Ok(if cell.continuation {
                        Cell::spacer(style)
                    } else {
                        Cell::with_text(CellText::from_str(&cell.text), style, cell.width)
                    })
                })
                .collect(),
            Self::Compact {
                text,
                ends,
                wide,
                style,
                runs,
                len,
            } => {
                let malformed = FrameApplyError::Malformed("compact line inconsistent");
                if !ends.is_empty() && ends.len() != *len as usize {
                    return Err(malformed);
                }
                let mut cells = Vec::with_capacity(*len as usize + wide.len());
                let (mut runs, mut wide) = (runs.iter().peekable(), wide.iter().peekable());
                let mut chars = text.char_indices();
                let (mut current, mut start) = (style_id(*style)?, 0_usize);
                for index in 0..*len {
                    if let Some((_, run_style)) = runs.next_if(|(first, _)| *first == index) {
                        current = style_id(*run_style)?;
                    }
                    let end = if ends.is_empty() {
                        chars
                            .next()
                            .map(|(offset, ch)| offset + ch.len_utf8())
                            .ok_or(malformed)?
                    } else {
                        ends[index as usize] as usize
                    };
                    let cell_text = text.get(start..end).ok_or(malformed)?;
                    let two = wide.next_if(|cell| **cell == index).is_some();
                    cells.push(Cell::with_text(
                        CellText::from_str(cell_text),
                        current,
                        if two { 2 } else { 1 },
                    ));
                    if two {
                        cells.push(Cell::spacer(current));
                    }
                    start = end;
                }
                if start != text.len() || runs.next().is_some() || wide.next().is_some() {
                    return Err(malformed);
                }
                Ok(cells)
            }
        }
    }
}
