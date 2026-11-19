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
//!
//! Frames are bytes, written straight into a caller's buffer ([`TerminalGrid::write_frame_since`])
//! and applied straight from it ([`TerminalGrid::apply_frame`]), each row decoded into the
//! storage of the row it replaces: building and applying a frame allocates nothing once the
//! buffers are warm, beyond the history lines a replica keeps. Rows, lines and moves are
//! varint-encoded by hand (they are the volume); new palette entries and the terminal state,
//! small and rare, are `bmux_codec` positional blobs.

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

/// The frame encoding written and accepted.
const FRAME_VERSION: u8 = 1;

const FLAG_RESET: u8 = 1;
const FLAG_HISTORY_CLEARED: u8 = 2;
const FLAG_PENDING_HISTORY: u8 = 4;

const LINE_COMPACT: u8 = 0;
const LINE_CELLS: u8 = 1;

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

/// What a frame written by [`TerminalGrid::write_frame_since`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSummary {
    /// The replica's cursor once it applies the frame.
    pub next: ReplicaCursor,
    /// The frame replaces the replica's whole state.
    pub reset: bool,
    pub moves: usize,
    pub rows: usize,
    pub history: usize,
    pub styles: usize,
}

impl FrameSummary {
    /// Whether the frame changes nothing but terminal state.
    #[must_use]
    pub const fn is_state_only(&self) -> bool {
        !self.reset && self.moves == 0 && self.rows == 0 && self.history == 0 && self.styles == 0
    }
}

/// What applying a frame did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameApplied {
    /// The frame replaced the replica's whole state.
    pub reset: bool,
}

/// Why a frame could not be applied: the replica must be sent a reset frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameApplyError {
    #[error("frame base does not match the replica")]
    BaseMismatch,
    #[error("frame is malformed: {0}")]
    Malformed(&'static str),
}

/// Which screen a row or move belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameScreen {
    Main,
    Alternate,
}

impl FrameScreen {
    const fn code(self) -> u8 {
        match self {
            Self::Main => 0,
            Self::Alternate => 1,
        }
    }

    const fn from_code(code: u8) -> Result<Self, FrameApplyError> {
        match code {
            0 => Ok(Self::Main),
            1 => Ok(Self::Alternate),
            _ => Err(FrameApplyError::Malformed("unknown screen")),
        }
    }
}

/// Rows `top..=bottom` of a screen moved by `rows`, positive toward `top`. Rows moved out of
/// the region are dropped (history arrives as lines); rows exposed are always sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameMove {
    screen: FrameScreen,
    top: u16,
    bottom: u16,
    rows: i32,
}

/// Autowrap and the pending wrap, live and saved with the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FrameWrap {
    autowrap: bool,
    pending: bool,
    saved_pending: bool,
}

/// The grid's terminal state, sent whole in every frame (it is small), written from a borrowed
/// view of the grid and read back owned: both serialize the same fields in the same order.
#[derive(Serialize)]
struct FrameStateRef<'a> {
    alternate: bool,
    cursor: CursorSnapshot,
    saved_cursor: CursorSnapshot,
    wrap: FrameWrap,
    characters: crate::CharacterState,
    saved_characters: crate::CharacterState,
    current_style: Style,
    terminal_state: &'a TerminalStateSnapshot,
    scroll_region: Option<ScrollRegionSnapshot>,
    protocol: &'a crate::ProtocolState,
    total_scrolled_rows: u64,
    history_truncated: bool,
}

#[derive(Deserialize)]
struct FrameState {
    alternate: bool,
    cursor: CursorSnapshot,
    saved_cursor: CursorSnapshot,
    wrap: FrameWrap,
    characters: crate::CharacterState,
    saved_characters: crate::CharacterState,
    current_style: Style,
    terminal_state: TerminalStateSnapshot,
    scroll_region: Option<ScrollRegionSnapshot>,
    protocol: crate::ProtocolState,
    total_scrolled_rows: u64,
    history_truncated: bool,
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

    /// Write into `out` (cleared first) the frame that brings a replica at `base` level with
    /// this grid, sealing the changes so far into an epoch, and say what it holds. At most
    /// `max_history_lines` history lines are sent; older ones the replica lacks are skipped, and
    /// it marks its history truncated.
    pub fn write_frame_since(
        &mut self,
        base: ReplicaCursor,
        max_history_lines: usize,
        out: &mut Vec<u8>,
    ) -> FrameSummary {
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
        let (history_cleared, history_from) = match self.damage.history_cleared {
            _ if reset => (true, 0),
            Some((epoch, pushed)) if epoch > since => (true, pushed.max(base.history)),
            _ => (false, base.history),
        };
        let unsent = usize::try_from(self.damage.history_pushed.saturating_sub(history_from))
            .unwrap_or(usize::MAX);
        let history = unsent.min(self.main_history.len()).min(max_history_lines);
        let pending = reset || self.damage.pending_at > since;
        let next = ReplicaCursor {
            epoch: sealed,
            history: self.damage.history_pushed,
            styles: u32::try_from(self.palette.len()).unwrap_or(u32::MAX),
        };

        // Size the buffer once: each changed row or line takes about its width (text and a few
        // bytes of runs), plus the header and state.
        let changed = self
            .main_rows
            .iter()
            .chain(self.alt_rows.iter())
            .filter(|row| row.stamp > since)
            .count();
        out.clear();
        out.reserve(512 + (changed + history + usize::from(pending)) * (self.width + 16));
        out.push(FRAME_VERSION);
        out.push(
            (u8::from(reset) * FLAG_RESET)
                | (u8::from(history_cleared) * FLAG_HISTORY_CLEARED)
                | (u8::from(pending) * FLAG_PENDING_HISTORY),
        );
        for cursor in [base, next] {
            put(out, cursor.epoch);
            put(out, cursor.history);
            put(out, u64::from(cursor.styles));
        }
        put(out, self.revision);
        put(out, self.content_revision);
        put(out, self.width as u64);
        put(out, self.height as u64);
        let styles = &self.palette.styles()[if reset { 0 } else { base.styles as usize }..];
        put_blob(out, &styles);

        let mut moves = 0;
        let moves_at = out.len();
        put_placeholder(out);
        if !reset {
            for (_, movement) in self.damage.moves.iter().filter(|(epoch, _)| *epoch > since) {
                out.push(movement.screen.code());
                put(out, u64::from(movement.top));
                put(out, u64::from(movement.bottom));
                put_signed(out, i64::from(movement.rows));
                moves += 1;
            }
        }
        patch(out, moves_at, moves);

        let rows = self.write_rows_since(since, out);

        put(out, history as u64);
        for line in self
            .main_history
            .iter()
            .skip(self.main_history.len() - history)
        {
            write_line(out, &line.cells());
        }
        put(out, self.main_history.len() as u64);
        if pending {
            write_line(out, &self.pending_history_cells);
        }
        put_blob(out, &self.frame_state());
        FrameSummary {
            next,
            reset,
            moves,
            rows,
            history,
            styles: styles.len(),
        }
    }

    /// Write every row of either screen changed since epoch `since`, after their count.
    fn write_rows_since(&self, since: u64, out: &mut Vec<u8>) -> usize {
        let mut rows = 0;
        let rows_at = out.len();
        put_placeholder(out);
        let main = self.main_rows.iter().map(|row| (FrameScreen::Main, row));
        let alternate = self
            .alt_rows
            .iter()
            .map(|row| (FrameScreen::Alternate, row));
        let mut index = [0_u64; 2];
        for (screen, row) in main.chain(alternate) {
            let slot = &mut index[usize::from(screen.code())];
            if row.stamp > since {
                out.push(screen.code());
                put(out, *slot);
                out.push(u8::from(row.wrapped));
                write_line(out, &row.cells);
                rows += 1;
            }
            *slot += 1;
        }
        patch(out, rows_at, rows);
        rows
    }

    fn frame_state(&self) -> FrameStateRef<'_> {
        let cursor = |cursor: Cursor| CursorSnapshot {
            row: u16::try_from(cursor.row).unwrap_or(u16::MAX),
            col: u16::try_from(cursor.col).unwrap_or(u16::MAX),
            visible: cursor.visible,
        };
        FrameStateRef {
            alternate: self.mode == GridMode::Alternate,
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
            terminal_state: &self.terminal_state,
            scroll_region: self
                .scroll_region
                .map(|(top, bottom)| ScrollRegionSnapshot {
                    top: u16::try_from(top).unwrap_or(u16::MAX),
                    bottom: u16::try_from(bottom).unwrap_or(u16::MAX),
                }),
            protocol: &self.protocol,
            total_scrolled_rows: self.total_scrolled_rows,
            history_truncated: self.history_truncated,
        }
    }

    /// Apply a frame written by [`Self::write_frame_since`] for this replica's `cursor`, and
    /// advance it. A replica that sees an error must be sent a reset frame.
    ///
    /// # Errors
    /// Returns an error when the frame is not for `cursor` or is malformed.
    pub fn apply_frame(
        &mut self,
        frame: &[u8],
        cursor: &mut ReplicaCursor,
    ) -> Result<FrameApplied, FrameApplyError> {
        let mut input = Reader::new(frame);
        if input.byte()? != FRAME_VERSION {
            return Err(FrameApplyError::Malformed("unknown frame version"));
        }
        let flags = input.byte()?;
        let reset = flags & FLAG_RESET != 0;
        let base = input.cursor()?;
        let next = input.cursor()?;
        if !reset && base != *cursor {
            return Err(FrameApplyError::BaseMismatch);
        }
        let revision = input.number()?;
        let content_revision = input.number()?;
        let width = input.size()?;
        let height = input.size()?;
        if width == 0 || height == 0 {
            return Err(FrameApplyError::Malformed("zero dimensions"));
        }
        let styles: Vec<Style> = input.blob()?;
        if reset {
            self.palette = StylePalette::from_styles(styles);
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
            for style in styles {
                self.palette.push(style);
            }
        }
        let palette = self.palette.len();
        for _ in 0..input.count()? {
            let movement = FrameMove {
                screen: FrameScreen::from_code(input.byte()?)?,
                top: input.narrow()?,
                bottom: input.narrow()?,
                rows: i32::try_from(input.signed()?)
                    .map_err(|_| FrameApplyError::Malformed("move too long"))?,
            };
            self.apply_move(movement)?;
        }
        for _ in 0..input.count()? {
            let screen = FrameScreen::from_code(input.byte()?)?;
            let index = input.size()?;
            let wrapped = input.byte()? != 0;
            let rows: &mut dyn std::ops::IndexMut<usize, Output = PhysicalRow> = match screen {
                FrameScreen::Main => &mut self.main_rows,
                FrameScreen::Alternate => &mut self.alt_rows,
            };
            if index >= height {
                return Err(FrameApplyError::Malformed("row outside the screen"));
            }
            let target = &mut rows[index];
            read_line(&mut input, &mut target.cells, palette)?;
            target.wrapped = wrapped;
            target.changed = true;
            target.fresh = true;
        }
        self.apply_history(&mut input, flags, base, next, palette, reset)?;
        let state: FrameState = input.blob()?;
        self.apply_state(&state)?;
        if !input.is_empty() {
            return Err(FrameApplyError::Malformed("trailing bytes"));
        }
        self.revision = revision;
        self.content_revision = content_revision;
        *cursor = next;
        Ok(FrameApplied { reset })
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
                self.main_rows.make_contiguous();
                rotate_region(
                    &mut self.main_rows.as_mut_slices().0[top..=bottom],
                    shift,
                    up,
                );
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

    fn apply_history(
        &mut self,
        input: &mut Reader<'_>,
        flags: u8,
        base: ReplicaCursor,
        next: ReplicaCursor,
        palette: usize,
        reset: bool,
    ) -> Result<(), FrameApplyError> {
        if flags & FLAG_HISTORY_CLEARED != 0 {
            let retained: usize = self
                .main_history
                .iter()
                .map(LogicalLine::history_bytes)
                .sum();
            self.history_bytes = self.history_bytes.saturating_sub(retained);
            self.main_history.clear();
            self.main_history_projected_rows = OnceLock::from(0);
        }
        let lines = input.size()?;
        let mut cells = Vec::new();
        for _ in 0..lines {
            read_line(input, &mut cells, palette)?;
            self.push_history_line(LogicalLine::from_slice(&cells));
        }
        let retained = input.size()?;
        // Lines the grid pushed that this frame does not carry leave a gap in the replica's
        // history; a reset frame starts it over, marked only by what it skips.
        let skipped = if reset {
            lines < retained
        } else {
            next.history.saturating_sub(base.history) > lines as u64
        };
        if reset {
            self.history_truncated = false;
        }
        self.history_truncated |= skipped;
        while self.main_history.len() > retained {
            let Some(line) = self.main_history.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(line.history_bytes());
            if let Some(count) = self.main_history_projected_rows.get_mut() {
                *count = count.saturating_sub(line.projected_row_count(self.width));
            }
        }
        if flags & FLAG_PENDING_HISTORY != 0 {
            let mut pending = std::mem::take(&mut self.pending_history_cells);
            self.history_bytes = self
                .history_bytes
                .saturating_sub(history_cells_bytes(&pending));
            read_line(input, &mut pending, palette)?;
            self.history_bytes = self
                .history_bytes
                .saturating_add(history_cells_bytes(&pending));
            self.pending_history_cells = pending;
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
        let mode = if state.alternate {
            GridMode::Alternate
        } else {
            GridMode::Main
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
        self.terminal_state.clone_from(&state.terminal_state);
        self.scroll_region = state
            .scroll_region
            .map(|region| (usize::from(region.top), usize::from(region.bottom)));
        self.protocol = state.protocol;
        self.total_scrolled_rows = state.total_scrolled_rows;
        self.history_truncated |= state.history_truncated;
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

/// Write `cells` as a line: compactly (text, style runs, wide cells, text ends only when some
/// cell is not one character) when every cell is one or two columns wide, each two-column cell
/// followed by a continuation of its style and no other continuation; else cell by cell.
fn write_line(out: &mut Vec<u8>, cells: &[Cell]) {
    let Some(shape) = compact_shape(cells) else {
        out.push(LINE_CELLS);
        put(out, cells.len() as u64);
        for cell in cells {
            out.push(u8::from(cell.wide_continuation));
            put(out, u64::from(cell.width));
            put(out, u64::from(cell.style.0));
            let text = cell.text();
            put(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
        return;
    };
    let logical = || cells.iter().filter(|cell| !cell.wide_continuation);
    out.push(LINE_COMPACT);
    put(out, shape.len as u64);
    put(out, u64::from(cells.first().map_or(0, |cell| cell.style.0)));
    put(out, shape.runs as u64);
    put(out, shape.wide as u64);
    out.push(u8::from(!shape.single));
    put(out, shape.text as u64);
    for cell in logical() {
        out.extend_from_slice(cell.text().as_bytes());
    }
    let mut current = cells.first().map_or(0, |cell| cell.style.0);
    for (index, cell) in (0_u64..).zip(logical()) {
        if cell.style.0 != current {
            put(out, index);
            put(out, u64::from(cell.style.0));
            current = cell.style.0;
        }
    }
    for (index, cell) in (0_u64..).zip(logical()) {
        if cell.width == 2 {
            put(out, index);
        }
    }
    if !shape.single {
        let mut end = 0;
        for cell in logical() {
            end += cell.text().len() as u64;
            put(out, end);
        }
    }
}

/// What the compact form of a line holds, or `None` when it cannot hold the cells exactly.
struct CompactShape {
    len: usize,
    runs: usize,
    wide: usize,
    text: usize,
    single: bool,
}

fn compact_shape(cells: &[Cell]) -> Option<CompactShape> {
    let mut shape = CompactShape {
        len: 0,
        runs: 0,
        wide: 0,
        text: 0,
        single: true,
    };
    let mut current = cells.first().map(|cell| cell.style);
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
            shape.wide += 1;
        }
        let text = cell.text();
        shape.single &= text.len() == 1 || text.chars().count() == 1;
        shape.text += text.len();
        if current != Some(cell.style) {
            shape.runs += 1;
            current = Some(cell.style);
        }
        shape.len += 1;
    }
    Some(shape)
}

/// Read a line written by [`write_line`] into `cells` (cleared first, its storage kept), its
/// styles checked against a palette of `palette` entries.
fn read_line(
    input: &mut Reader<'_>,
    cells: &mut Vec<Cell>,
    palette: usize,
) -> Result<(), FrameApplyError> {
    let style_id = |id: u64| match u32::try_from(id) {
        Ok(id) if (id as usize) < palette.max(1) => Ok(StyleId(id)),
        _ => Err(FrameApplyError::Malformed("style outside the palette")),
    };
    let malformed = FrameApplyError::Malformed("line inconsistent");
    cells.clear();
    match input.byte()? {
        LINE_CELLS => {
            for _ in 0..input.size()? {
                let continuation = input.byte()? != 0;
                let width = u8::try_from(input.number()?).map_err(|_| malformed)?;
                let style = style_id(input.number()?)?;
                let length = input.size()?;
                let text = std::str::from_utf8(input.take(length)?).map_err(|_| malformed)?;
                cells.push(if continuation {
                    Cell::spacer(style)
                } else {
                    Cell::with_text(CellText::from_str(text), style, width)
                });
            }
            Ok(())
        }
        LINE_COMPACT => {
            let len = input.size()?;
            let mut style = style_id(input.number()?)?;
            let (runs, wide) = (input.size()?, input.size()?);
            let ends = input.byte()? != 0;
            let length = input.size()?;
            let text = std::str::from_utf8(input.take(length)?).map_err(|_| malformed)?;
            // The run, wide and end lists follow one another; read them side by side.
            let mut run_list = input.clone();
            input.skip_numbers(runs * 2)?;
            let mut wide_list = input.clone();
            input.skip_numbers(wide)?;
            let mut end_list = input.clone();
            if ends {
                input.skip_numbers(len)?;
            }
            let (mut runs_left, mut wide_left) = (runs, wide);
            let mut next_run = None;
            if runs_left > 0 {
                next_run = Some((run_list.number()?, run_list.number()?));
                runs_left -= 1;
            }
            let mut next_wide = None;
            if wide_left > 0 {
                next_wide = Some(wide_list.number()?);
                wide_left -= 1;
            }
            cells.reserve(len + wide);
            let mut chars = text.char_indices();
            let mut start = 0_usize;
            for index in 0..len as u64 {
                if let Some((first, run_style)) = next_run
                    && first == index
                {
                    style = style_id(run_style)?;
                    next_run = None;
                    if runs_left > 0 {
                        next_run = Some((run_list.number()?, run_list.number()?));
                        runs_left -= 1;
                    }
                }
                let end = if ends {
                    usize::try_from(end_list.number()?).map_err(|_| malformed)?
                } else {
                    chars
                        .next()
                        .map(|(offset, ch)| offset + ch.len_utf8())
                        .ok_or(malformed)?
                };
                let cell_text = text.get(start..end).ok_or(malformed)?;
                let two = next_wide == Some(index);
                if two {
                    next_wide = None;
                    if wide_left > 0 {
                        next_wide = Some(wide_list.number()?);
                        wide_left -= 1;
                    }
                }
                cells.push(Cell::with_text(
                    CellText::from_str(cell_text),
                    style,
                    if two { 2 } else { 1 },
                ));
                if two {
                    cells.push(Cell::spacer(style));
                }
                start = end;
            }
            if start != text.len() || next_run.is_some() || next_wide.is_some() {
                return Err(malformed);
            }
            Ok(())
        }
        _ => Err(FrameApplyError::Malformed("unknown line form")),
    }
}

/// Append `value` as a LEB128 varint.
fn put(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Append `value` zigzag-encoded.
fn put_signed(out: &mut Vec<u8>, value: i64) {
    put(out, ((value << 1) ^ (value >> 63)).cast_unsigned());
}

/// Reserve room for a count written later by [`patch`]: a fixed four bytes.
fn put_placeholder(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0; 4]);
}

fn patch(out: &mut [u8], at: usize, count: usize) {
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    out[at..at + 4].copy_from_slice(&count.to_le_bytes());
}

/// Append `value` as a positional `bmux_codec` blob after its length (four bytes).
fn put_blob<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) {
    let at = out.len();
    put_placeholder(out);
    // Plain data with no failing serializations.
    let _ = bmux_codec::append_positional(value, out);
    let length = out.len() - at - 4;
    patch(out, at, length);
}

/// A frame being read.
#[derive(Clone)]
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn byte(&mut self) -> Result<u8, FrameApplyError> {
        let (&first, rest) = self
            .bytes
            .split_first()
            .ok_or(FrameApplyError::Malformed("frame ends early"))?;
        self.bytes = rest;
        Ok(first)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], FrameApplyError> {
        if length > self.bytes.len() {
            return Err(FrameApplyError::Malformed("frame ends early"));
        }
        let (taken, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(taken)
    }

    fn number(&mut self) -> Result<u64, FrameApplyError> {
        let mut value = 0_u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(FrameApplyError::Malformed("number too long"))
    }

    fn signed(&mut self) -> Result<i64, FrameApplyError> {
        let value = self.number()?;
        Ok((value >> 1).cast_signed() ^ -((value & 1).cast_signed()))
    }

    fn size(&mut self) -> Result<usize, FrameApplyError> {
        usize::try_from(self.number()?).map_err(|_| FrameApplyError::Malformed("size too large"))
    }

    fn narrow(&mut self) -> Result<u16, FrameApplyError> {
        u16::try_from(self.number()?).map_err(|_| FrameApplyError::Malformed("value too large"))
    }

    fn count(&mut self) -> Result<usize, FrameApplyError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    }

    fn cursor(&mut self) -> Result<ReplicaCursor, FrameApplyError> {
        Ok(ReplicaCursor {
            epoch: self.number()?,
            history: self.number()?,
            styles: u32::try_from(self.number()?)
                .map_err(|_| FrameApplyError::Malformed("palette too large"))?,
        })
    }

    fn skip_numbers(&mut self, count: usize) -> Result<(), FrameApplyError> {
        for _ in 0..count {
            self.number()?;
        }
        Ok(())
    }

    fn blob<T: serde::de::DeserializeOwned>(&mut self) -> Result<T, FrameApplyError> {
        let length = self.count()?;
        bmux_codec::from_positional_bytes(self.take(length)?)
            .map_err(|_| FrameApplyError::Malformed("undecodable state"))
    }
}
