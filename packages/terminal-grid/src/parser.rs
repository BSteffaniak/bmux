use crate::delta::{GridDeltaApplyError, GridDeltaBatch};
use crate::model::{
    GridLimits, GridMode, MouseProtocolEncoding, MouseProtocolMode, ProtocolState, TerminalGrid,
    TerminalGridError,
};
use vte::{Params, Perform};

/// Streaming terminal parser plus structured grid state.
///
/// Unlike [`TerminalGrid::process`](crate::TerminalGrid::process), this type
/// owns the `vte` parser state and therefore preserves incomplete escape
/// sequences across PTY chunk boundaries.
pub struct TerminalGridStream {
    parser: vte::Parser,
    grid: TerminalGrid,
    pending_bytes: Vec<u8>,
}

impl TerminalGridStream {
    /// Create a new streaming parser and grid.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn new(width: u16, height: u16, limits: GridLimits) -> Result<Self, TerminalGridError> {
        Ok(Self::from_grid(TerminalGrid::new(width, height, limits)?))
    }

    /// Wrap an existing grid with a fresh parser state.
    #[must_use]
    pub fn from_grid(grid: TerminalGrid) -> Self {
        Self {
            parser: vte::Parser::new(),
            grid,
            pending_bytes: Vec::new(),
        }
    }

    /// Hydrate a stream from a structured snapshot, including parser-prefix
    /// bytes that were consumed by the source stream but had not completed a
    /// terminal sequence yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot hydrate a grid.
    pub fn from_snapshot(
        snapshot: &crate::snapshot::GridSnapshot,
        limits: GridLimits,
    ) -> Result<Self, TerminalGridError> {
        let mut stream = Self::from_grid(TerminalGrid::from_snapshot(snapshot, limits)?);
        if !snapshot.pending_bytes.is_empty() {
            let mut performer = GridPerformer {
                grid: &mut stream.grid,
            };
            stream
                .parser
                .advance(&mut performer, &snapshot.pending_bytes);
            stream.pending_bytes.clone_from(&snapshot.pending_bytes);
        }
        Ok(stream)
    }

    /// Borrow the structured grid.
    #[must_use]
    pub const fn grid(&self) -> &TerminalGrid {
        &self.grid
    }

    /// Mutably borrow the structured grid.
    pub fn grid_mut(&mut self) -> &mut TerminalGrid {
        &mut self.grid
    }

    /// Consume the stream and return the grid.
    #[must_use]
    pub fn into_grid(self) -> TerminalGrid {
        self.grid
    }

    /// Process one chunk of PTY output.
    pub fn process(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let revision = self.grid.revision();
        let mut previous_pending = std::mem::take(&mut self.pending_bytes);
        let previous_pending_len = previous_pending.len();
        self.pending_bytes = if previous_pending.is_empty() {
            trailing_incomplete_sequence(bytes)
        } else {
            previous_pending.extend_from_slice(bytes);
            trailing_incomplete_sequence(&previous_pending)
        };
        let mut performer = GridPerformer {
            grid: &mut self.grid,
        };
        self.parser.advance(&mut performer, bytes);
        // Parser prefixes are replicated state even when no terminal operation
        // completed. Do not mark visible content dirty for a prefix-only update.
        if self.pending_bytes != previous_pending[..previous_pending_len]
            && self.grid.revision() == revision
        {
            self.grid.bump_revision();
        }
    }

    /// Snapshot the grid plus parser-prefix bytes needed to continue a split
    /// terminal sequence from a newly hydrated stream.
    #[must_use]
    pub fn snapshot(&self, scrollback_offset: usize, rows: usize) -> crate::GridSnapshot {
        let mut snapshot = self.grid.snapshot(scrollback_offset, rows);
        snapshot.pending_bytes.clone_from(&self.pending_bytes);
        snapshot
    }

    /// Fallible snapshot capture including admission of parser-prefix storage.
    #[must_use]
    pub fn try_snapshot(
        &self,
        offset: usize,
        rows: usize,
        budget: usize,
    ) -> Option<crate::GridSnapshot> {
        let mut remaining = budget;
        let mut pending_bytes = Vec::new();
        crate::snapshot::reserve_snapshot_vec(
            &mut pending_bytes,
            self.pending_bytes.len(),
            &mut remaining,
        )?;
        pending_bytes.extend_from_slice(&self.pending_bytes);
        // Prefix storage coexists with every projection and wire allocation.
        // Charge returned capacity, not just its requested length.
        let mut snapshot = crate::GridSnapshot::try_from_grid(&self.grid, offset, rows, remaining)?;
        snapshot.pending_bytes = pending_bytes;
        Some(snapshot)
    }

    /// Apply a structured delta by rebuilding the stream from the resulting
    /// snapshot. This keeps parser-prefix state in sync with the producer.
    ///
    /// # Errors
    ///
    /// Returns an error when the delta does not apply to the current revision
    /// or the resulting snapshot is invalid.
    pub fn apply_delta(
        &mut self,
        delta: &GridDeltaBatch,
        limits: GridLimits,
    ) -> Result<(), TerminalGridStreamDeltaError> {
        self.validate_delta_before_capture(delta, limits)?;
        self.apply_validated_delta(delta, limits)
    }

    fn validate_delta_before_capture(
        &self,
        delta: &GridDeltaBatch,
        limits: GridLimits,
    ) -> Result<(), TerminalGridStreamDeltaError> {
        delta.validate_revisions(self.grid.revision(), self.grid.content_revision())?;
        delta.validate_geometry()?;
        delta.validate_replacement_indexes()?;
        if !delta.reset_rows
            && (usize::from(delta.width) != self.grid.width()
                || usize::from(delta.height) != self.grid.height()
                || usize::try_from(delta.scrollback_rows).ok()
                    != Some(self.grid.scrollback_rows_hint())
                || delta
                    .total_scrolled_rows
                    .is_some_and(|position| position != self.grid.total_scrolled_rows())
                || (delta.mode == "alternate") != (self.grid.mode() == GridMode::Alternate))
        {
            return Err(crate::GridDeltaApplyError::MissingReplacementRows.into());
        }
        delta.validate_sparse_indexes(self.grid.height())?;
        // Limits count completed logical lines, not their width-dependent
        // physical projection. A wrapped history prefix is retained separately.
        let retained_lines = if delta.mode == "alternate" {
            delta.main_rows.as_ref().map_or_else(
                || {
                    if self.grid.mode() == GridMode::Alternate {
                        self.grid.retained_history_line_count()
                    } else {
                        0
                    }
                },
                |rows| {
                    rows.iter()
                        .take(rows.len().saturating_sub(usize::from(delta.height)))
                        .filter(|row| !row.wrapped)
                        .count()
                },
            )
        } else if delta.reset_rows {
            delta
                .row_updates
                .iter()
                .take(
                    delta
                        .row_updates
                        .len()
                        .saturating_sub(usize::from(delta.height)),
                )
                .filter(|update| !update.row.wrapped)
                .count()
        } else {
            self.grid.retained_history_line_count()
        };
        if retained_lines > limits.scrollback_rows {
            return Err(TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: u32::try_from(retained_lines).unwrap_or(u32::MAX),
                reconstructed_rows: limits.scrollback_rows,
            });
        }
        if delta.reset_rows && delta.mode == "main" {
            let supplied_history = delta
                .row_updates
                .len()
                .saturating_sub(usize::from(delta.height));
            if supplied_history != usize::try_from(delta.scrollback_rows).unwrap_or(usize::MAX) {
                return Err(TerminalGridStreamDeltaError::IncompleteHistory {
                    expected_rows: delta.scrollback_rows,
                    reconstructed_rows: supplied_history,
                });
            }
        }
        Ok(())
    }

    fn apply_validated_delta(
        &mut self,
        delta: &GridDeltaBatch,
        limits: GridLimits,
    ) -> Result<(), TerminalGridStreamDeltaError> {
        // Preserve omitted alternate-screen backing in the initial capture,
        // rather than materializing a second snapshot of the same state.
        let preserve_main_history = !delta.reset_rows
            && delta.mode == "main"
            && self.grid.mode() == GridMode::Main
            && self.grid.scrollback_rows_hint() > 0;
        let snapshot_rows = if preserve_main_history
            || (delta.mode == "alternate"
                && delta.main_rows.is_none()
                && self.grid.mode() == GridMode::Alternate)
        {
            self.grid.main_row_count()
        } else {
            self.grid.height()
        };
        let mut snapshot = self.snapshot(0, snapshot_rows);
        // Sparse indexes are viewport-relative. Separate the history before
        // applying updates, then restore it without capturing the grid again.
        let mut retained = if preserve_main_history {
            let viewport = snapshot
                .rows
                .split_off(snapshot.rows.len().saturating_sub(self.grid.height()));
            std::mem::replace(&mut snapshot.rows, viewport)
        } else {
            Vec::new()
        };
        delta.apply_to_snapshot(&mut snapshot)?;
        if preserve_main_history {
            retained.append(&mut snapshot.rows);
            snapshot.rows = retained;
        }
        // A main-screen snapshot needs backing rows in addition to its viewport.
        // Reject obvious truncation before allocating a replacement grid; the
        // post-hydration check below also catches reflow and retention losses.
        let supplied_history = snapshot
            .rows
            .len()
            .saturating_sub(usize::from(snapshot.height));
        if snapshot.mode == "main"
            && supplied_history < usize::try_from(snapshot.scrollback_rows).unwrap_or(usize::MAX)
        {
            return Err(TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: snapshot.scrollback_rows,
                reconstructed_rows: supplied_history,
            });
        }
        let replacement = Self::from_snapshot(&snapshot, limits)?;
        if snapshot.mode == "alternate"
            && let Some(rows) = &snapshot.main_rows
            && replacement.grid.main_row_count() != rows.len()
        {
            return Err(TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: u32::try_from(
                    rows.len().saturating_sub(usize::from(snapshot.height)),
                )
                .unwrap_or(u32::MAX),
                reconstructed_rows: replacement
                    .grid
                    .main_row_count()
                    .saturating_sub(usize::from(snapshot.height)),
            });
        }
        if replacement.grid.scrollback_rows_hint()
            != usize::try_from(snapshot.scrollback_rows).unwrap_or(usize::MAX)
        {
            return Err(TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: snapshot.scrollback_rows,
                reconstructed_rows: replacement.grid.scrollback_rows_hint(),
            });
        }
        *self = replacement;
        Ok(())
    }

    /// Process one chunk and return a structured row delta when state changed.
    #[must_use]
    pub fn process_delta(&mut self, bytes: &[u8]) -> Option<GridDeltaBatch> {
        if bytes.is_empty() {
            return None;
        }
        let before_rows = if self.grid.mode() == GridMode::Alternate {
            self.grid.main_row_count()
        } else {
            self.grid.height()
        };
        let before = self.snapshot(0, before_rows);
        self.process(bytes);
        if self.grid.revision() == before.revision {
            return None;
        }
        let replacement_history = self.grid.mode() == GridMode::Main
            && (before.mode != "main"
                || usize::try_from(before.scrollback_rows).ok()
                    != Some(self.grid.scrollback_rows_hint())
                || before.total_scrolled_rows != Some(self.grid.total_scrolled_rows()));
        let rows = if replacement_history || self.grid.mode() == GridMode::Alternate {
            self.grid.main_row_count()
        } else {
            self.grid.height()
        };
        let after = self.snapshot(0, rows);
        GridDeltaBatch::between(&before, &after)
    }

    /// Resize the grid without computing a structured delta.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), TerminalGridError> {
        self.grid.resize(width, height)
    }

    /// Resize the grid and return a structured row delta when state changed.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn resize_delta(
        &mut self,
        width: u16,
        height: u16,
    ) -> Result<Option<GridDeltaBatch>, TerminalGridError> {
        if width == 0 || height == 0 {
            return Err(TerminalGridError::ZeroDimensions);
        }
        if self.grid.width() == usize::from(width) && self.grid.height() == usize::from(height) {
            return Ok(None);
        }
        let before = self.snapshot(0, self.grid.height());
        self.grid.resize(width, height)?;
        // Resizing can reflow or move retained rows into the viewport. A
        // replacement must carry that backing, not only the visible tail.
        let after = self.snapshot(0, self.grid.main_row_count());
        Ok(GridDeltaBatch::between(&before, &after))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolProcessOutcome {
    pub toggled_alternate: bool,
}

/// Streaming parser for terminal protocol/input hints without retaining pane cells.
pub struct TerminalProtocolTracker {
    parser: vte::Parser,
    protocol: ProtocolState,
    alternate_screen: bool,
    pending_bytes: Vec<u8>,
}

impl Default for TerminalProtocolTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalProtocolTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            protocol: ProtocolState::default(),
            alternate_screen: false,
            pending_bytes: Vec::new(),
        }
    }

    #[must_use]
    pub const fn protocol_state(&self) -> ProtocolState {
        self.protocol
    }

    #[must_use]
    pub const fn alternate_screen(&self) -> bool {
        self.alternate_screen
    }

    /// Replace protocol state and parser continuity at an authoritative watermark.
    /// `pending_bytes` must be the incomplete sequence prefix from that watermark.
    pub fn restore(
        &mut self,
        protocol: ProtocolState,
        alternate_screen: bool,
        pending_bytes: &[u8],
    ) {
        *self = Self::new();
        let _ = self.process(pending_bytes);
        self.protocol = protocol;
        self.alternate_screen = alternate_screen;
    }

    pub fn set_protocol_state(&mut self, protocol: ProtocolState) {
        self.protocol = protocol;
    }

    pub fn set_alternate_screen(&mut self, alternate_screen: bool) {
        self.alternate_screen = alternate_screen;
    }

    pub fn process(&mut self, bytes: &[u8]) -> ProtocolProcessOutcome {
        if bytes.is_empty() {
            return ProtocolProcessOutcome {
                toggled_alternate: false,
            };
        }
        self.pending_bytes = if self.pending_bytes.is_empty() {
            trailing_incomplete_sequence(bytes)
        } else {
            let mut continuity = std::mem::take(&mut self.pending_bytes);
            continuity.extend_from_slice(bytes);
            trailing_incomplete_sequence(&continuity)
        };
        let mut performer = ProtocolPerformer {
            protocol: &mut self.protocol,
            alternate_screen: &mut self.alternate_screen,
            toggled_alternate: false,
        };
        self.parser.advance(&mut performer, bytes);
        ProtocolProcessOutcome {
            toggled_alternate: performer.toggled_alternate,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalGridStreamDeltaError {
    /// Applying the update would discard retained history during hydration.
    #[error(
        "delta history is incomplete: expected {expected_rows} rows, reconstructed {reconstructed_rows}"
    )]
    IncompleteHistory {
        expected_rows: u32,
        reconstructed_rows: usize,
    },
    #[error(transparent)]
    Delta(#[from] GridDeltaApplyError),
    #[error(transparent)]
    Grid(#[from] TerminalGridError),
}

pub(crate) fn process(grid: &mut TerminalGrid, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    let mut performer = GridPerformer { grid };
    parser.advance(&mut performer, bytes);
}

struct ProtocolPerformer<'a> {
    protocol: &'a mut ProtocolState,
    alternate_screen: &'a mut bool,
    toggled_alternate: bool,
}

impl ProtocolPerformer<'_> {
    fn set_alternate_screen(&mut self, enabled: bool) {
        if *self.alternate_screen != enabled {
            *self.alternate_screen = enabled;
            self.toggled_alternate = true;
        }
    }

    fn set_mouse_tracking_mode(&mut self, mode: MouseProtocolMode, enabled: bool) {
        match mode {
            MouseProtocolMode::None => {}
            MouseProtocolMode::Press => self.protocol.mouse_x10 = enabled,
            MouseProtocolMode::PressRelease => self.protocol.mouse_press_release = enabled,
            MouseProtocolMode::ButtonMotion => self.protocol.mouse_button_motion = enabled,
            MouseProtocolMode::AnyMotion => self.protocol.mouse_any_motion = enabled,
        }
    }

    fn set_mouse_encoding(&mut self, encoding: MouseProtocolEncoding, enabled: bool) {
        match encoding {
            MouseProtocolEncoding::Default => {}
            MouseProtocolEncoding::Utf8 => self.protocol.mouse_utf8 = enabled,
            MouseProtocolEncoding::Sgr => self.protocol.mouse_sgr = enabled,
        }
    }
}

impl Perform for ProtocolPerformer<'_> {
    fn print(&mut self, _c: char) {}

    fn execute(&mut self, _byte: u8) {}

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || intermediates != *b"?" || !matches!(action, 'h' | 'l') {
            return;
        }
        let enabled = action == 'h';
        for value in flatten_params(params) {
            match value {
                1 => self.protocol.application_cursor = enabled,
                9 => self.set_mouse_tracking_mode(MouseProtocolMode::Press, enabled),
                47 | 1047 | 1049 => self.set_alternate_screen(enabled),
                1000 => self.set_mouse_tracking_mode(MouseProtocolMode::PressRelease, enabled),
                1002 => self.set_mouse_tracking_mode(MouseProtocolMode::ButtonMotion, enabled),
                1003 => self.set_mouse_tracking_mode(MouseProtocolMode::AnyMotion, enabled),
                1005 => self.set_mouse_encoding(MouseProtocolEncoding::Utf8, enabled),
                1006 => self.set_mouse_encoding(MouseProtocolEncoding::Sgr, enabled),
                1015 => self.protocol.mouse_urxvt = enabled,
                2004 => self.protocol.bracketed_paste = enabled,
                _ => {}
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'=' => self.protocol.application_keypad = true,
            b'>' => self.protocol.application_keypad = false,
            b'c' => {
                *self.protocol = ProtocolState::default();
                self.set_alternate_screen(false);
            }
            _ => {}
        }
    }
}

struct GridPerformer<'a> {
    grid: &'a mut TerminalGrid,
}

impl Perform for GridPerformer<'_> {
    fn print(&mut self, c: char) {
        let ch = self.grid.characters.translate(c);
        self.grid.characters.last = Some(ch);
        self.grid.print_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.grid.linefeed(),
            b'\r' => self.grid.carriage_return(),
            0x08 => self.grid.backspace(),
            0x0e | 0x0f => {
                self.grid.characters.active = usize::from(byte == 0x0e);
                self.grid.bump_revision();
            }
            b'\t' => self.grid.tab(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    #[allow(
        clippy::too_many_lines,
        reason = "single CSI dispatcher keeps terminal escape handling in one explicit state-machine branch"
    )]
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let values = flatten_params(params);
        match action {
            'A' => self
                .grid
                .move_cursor_relative(-one_based(values.first()).cast_signed(), 0),
            'B' => self
                .grid
                .move_cursor_relative(one_based(values.first()).cast_signed(), 0),
            'C' => self
                .grid
                .move_cursor_relative(0, one_based(values.first()).cast_signed()),
            'D' => self
                .grid
                .move_cursor_relative(0, -one_based(values.first()).cast_signed()),
            'd' => self.grid.move_cursor_to(
                one_based(values.first()).saturating_sub(1),
                self.grid.cursor().col,
            ),
            'b' => {
                if let Some(ch) = self.grid.characters.last {
                    for _ in 0..one_based(values.first()) {
                        self.grid.print_char(ch);
                    }
                }
            }
            'G' | '`' => self.grid.move_cursor_to(
                self.grid.cursor().row,
                one_based(values.first()).saturating_sub(1),
            ),
            'H' | 'f' => {
                let row = one_based(values.first()).saturating_sub(1);
                let col = one_based(values.get(1)).saturating_sub(1);
                self.grid.move_cursor_to(row, col);
            }
            'E' => {
                self.grid
                    .move_cursor_relative(one_based(values.first()).cast_signed(), 0);
                self.grid.carriage_return();
            }
            'F' => {
                self.grid
                    .move_cursor_relative(-one_based(values.first()).cast_signed(), 0);
                self.grid.carriage_return();
            }
            'J' => self.grid.erase_display(default_zero(values.first())),
            'K' => self.grid.erase_line(default_zero(values.first())),
            'L' => self.grid.insert_blank_lines(one_based(values.first())),
            'M' => self.grid.delete_lines(one_based(values.first())),
            'P' => self.grid.delete_chars(one_based(values.first())),
            'S' => {
                let (_, bottom) = self
                    .grid
                    .scroll_region()
                    .unwrap_or_else(|| (0, self.grid.height().saturating_sub(1)));
                self.grid
                    .scroll_region_up(0, bottom, one_based(values.first()));
            }
            'T' => {
                let (_, bottom) = self
                    .grid
                    .scroll_region()
                    .unwrap_or_else(|| (0, self.grid.height().saturating_sub(1)));
                self.grid
                    .scroll_region_down(0, bottom, one_based(values.first()));
            }
            'X' => self.grid.erase_chars(one_based(values.first())),
            '@' => self.grid.insert_blank_chars(one_based(values.first())),
            'm' => self.grid.set_graphic_rendition(&values),
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),
            'r' => {
                if values.is_empty() {
                    self.grid.set_scroll_region(None, None);
                } else {
                    let top = one_based(values.first()).saturating_sub(1);
                    let bottom = one_based(values.get(1)).saturating_sub(1);
                    self.grid.set_scroll_region(Some(top), Some(bottom));
                }
            }
            'h' | 'l' if intermediates == *b"?" => {
                let enabled = action == 'h';
                for value in values {
                    match value {
                        1 => self.grid.set_application_cursor(enabled),
                        7 => self.grid.set_autowrap(enabled),
                        9 => self
                            .grid
                            .set_mouse_tracking_mode(MouseProtocolMode::Press, enabled),
                        25 => self.grid.set_cursor_visible(enabled),
                        47 | 1047 => self.grid.set_mode(if enabled {
                            GridMode::Alternate
                        } else {
                            GridMode::Main
                        }),
                        1049 => {
                            if enabled {
                                self.grid.save_cursor();
                                self.grid.set_mode(GridMode::Alternate);
                            } else {
                                self.grid.set_mode(GridMode::Main);
                                self.grid.restore_cursor();
                            }
                        }
                        1000 => self
                            .grid
                            .set_mouse_tracking_mode(MouseProtocolMode::PressRelease, enabled),
                        1002 => self
                            .grid
                            .set_mouse_tracking_mode(MouseProtocolMode::ButtonMotion, enabled),
                        1003 => self
                            .grid
                            .set_mouse_tracking_mode(MouseProtocolMode::AnyMotion, enabled),
                        1005 => self
                            .grid
                            .set_mouse_encoding(MouseProtocolEncoding::Utf8, enabled),
                        1006 => self
                            .grid
                            .set_mouse_encoding(MouseProtocolEncoding::Sgr, enabled),
                        1015 => self.grid.set_mouse_urxvt_encoding(enabled),
                        2004 => self.grid.set_bracketed_paste(enabled),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        if let [slot @ (b'(' | b')')] = intermediates {
            match byte {
                b'0' | b'B' => {
                    self.grid.characters.graphics[usize::from(*slot == b')')] = byte == b'0';
                    self.grid.bump_revision();
                }
                _ => {}
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'D' => self.grid.linefeed(),
            b'E' => {
                self.grid.linefeed();
                self.grid.carriage_return();
            }
            b'M' => self.grid.reverse_index(),
            b'=' => self.grid.set_application_keypad(true),
            b'>' => self.grid.set_application_keypad(false),
            b'c' => self.grid.reset(),
            _ => {}
        }
    }
}

fn flatten_params(params: &Params) -> Vec<i64> {
    let mut values = Vec::new();
    for param in params {
        let mut pushed = false;
        for subparam in param {
            values.push(i64::from(*subparam));
            pushed = true;
        }
        if !pushed {
            values.push(0);
        }
    }
    values
}

fn one_based(value: Option<&i64>) -> usize {
    let value = value.copied().unwrap_or(1);
    if value <= 0 {
        1
    } else {
        usize::try_from(value).unwrap_or(usize::MAX)
    }
}

fn default_zero(value: Option<&i64>) -> usize {
    value
        .copied()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

fn trailing_incomplete_sequence(bytes: &[u8]) -> Vec<u8> {
    // Earlier invalid bytes do not invalidate an incomplete character at the
    // end of the chunk. Inspect only the final leading byte and its suffix.
    let utf8_window_start = bytes.len().saturating_sub(4);
    let utf8_start = bytes[utf8_window_start..]
        .iter()
        .rposition(|byte| byte & 0xc0 != 0x80)
        .map_or(bytes.len(), |index| utf8_window_start + index);
    let utf8_pending_start = match std::str::from_utf8(&bytes[utf8_start..]) {
        Err(error) if error.error_len().is_none() => utf8_start + error.valid_up_to(),
        Ok(_) | Err(_) => bytes.len(),
    };
    let esc_pending_start = bytes
        .iter()
        .rposition(|byte| *byte == 0x1b)
        .filter(|position| !escape_sequence_complete(&bytes[*position..]));
    let start = esc_pending_start
        .into_iter()
        .chain((utf8_pending_start < bytes.len()).then_some(utf8_pending_start))
        .min();
    start.map_or_else(Vec::new, |start| bytes[start..].to_vec())
}

fn escape_sequence_complete(sequence: &[u8]) -> bool {
    let Some((&first, rest)) = sequence.split_first() else {
        return true;
    };
    if first != 0x1b {
        return true;
    }
    let Some((&next, rest)) = rest.split_first() else {
        return false;
    };
    match next {
        b'[' => rest.iter().any(|byte| (0x40..=0x7e).contains(byte)),
        b']' => has_bel_or_string_terminator(rest),
        b'P' | b'_' | b'^' | b'X' => has_string_terminator(rest),
        0x20..=0x2f => rest.iter().any(|byte| (0x30..=0x7e).contains(byte)),
        _ => true,
    }
}

fn has_bel_or_string_terminator(bytes: &[u8]) -> bool {
    bytes.contains(&0x07) || has_string_terminator(bytes)
}

fn has_string_terminator(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|window| window == [0x1b, b'\\'])
}

#[cfg(test)]
mod tests {
    use crate::model::{GridLimits, TerminalGrid};
    use crate::parser::TerminalGridStream;

    #[test]
    fn budgeted_prefix_capture_preserves_pending_sequence_on_rejection() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        stream.process(b"hello\x1b[31");
        let expected = stream.snapshot(0, 2);
        assert!(!expected.pending_bytes.is_empty());
        assert!(
            stream
                .try_snapshot(0, 2, expected.pending_bytes.len() - 1)
                .is_none()
        );
        assert_eq!(stream.snapshot(0, 2), expected);
        let accepted = stream.try_snapshot(0, 2, 10_000).unwrap();
        assert_eq!(accepted, expected);
        let prefix_capacity = accepted.pending_bytes.capacity();
        let grid_minimum = (0..10_000)
            .find(|&budget| {
                crate::GridSnapshot::try_from_grid(stream.grid(), 0, 2, budget).is_some()
            })
            .unwrap();
        assert!(
            stream
                .try_snapshot(0, 2, grid_minimum + prefix_capacity - 1)
                .is_none()
        );
        assert_eq!(
            stream
                .try_snapshot(0, 2, grid_minimum + prefix_capacity)
                .unwrap(),
            expected
        );
        stream.process(b"mred");
        assert!(stream.snapshot(0, 2).pending_bytes.is_empty());
    }

    #[test]
    fn protocol_tracker_tracks_hints_without_rows() {
        let mut tracker = crate::TerminalProtocolTracker::new();
        let outcome = tracker.process(b"text\x1b[?1000h\x1b[?1006h\x1b[?1049h\x1b[?2004h\x1b=");

        assert!(outcome.toggled_alternate);
        assert!(tracker.alternate_screen());
        let protocol = tracker.protocol_state();
        assert_eq!(
            protocol.mouse_mode(),
            crate::model::MouseProtocolMode::PressRelease
        );
        assert_eq!(
            protocol.mouse_encoding(),
            crate::model::MouseProtocolEncoding::Sgr
        );
        assert!(protocol.application_keypad);
        assert!(protocol.bracketed_paste);

        let outcome = tracker.process(b"\x1b[?1049l\x1b[?1000l\x1b[?1006l\x1b[?2004l\x1b>");
        assert!(outcome.toggled_alternate);
        assert!(!tracker.alternate_screen());
        let protocol = tracker.protocol_state();
        assert_eq!(protocol.mouse_mode(), crate::model::MouseProtocolMode::None);
        assert_eq!(
            protocol.mouse_encoding(),
            crate::model::MouseProtocolEncoding::Default
        );
        assert!(!protocol.application_keypad);
        assert!(!protocol.bracketed_paste);
    }

    #[test]
    fn curses_borders_use_graphics_repeat_and_vertical_positioning() {
        let mut grid = TerminalGrid::new(8, 4, GridLimits::default()).unwrap();
        grid.process(
            b"\x1b(0lq\x1b[5bk\x1b[2d\rxx\x1b[8Gx\x1b[3d\rx\x1b[8Gx\x1b[4d\rmq\x1b[5bj\x1b(B",
        );
        let lines = crate::visible_text_lines(&grid, 0, 4);
        assert_eq!(lines[0], "┌──────┐");
        assert_eq!(lines[1], "││     │");
        assert_eq!(lines[2], "│      │");
        assert_eq!(lines[3], "└──────┘");
    }

    #[test]
    fn character_state_survives_chunks_snapshots_and_deltas() {
        let limits = GridLimits::default();
        let mut source = TerminalGridStream::new(16, 2, limits).unwrap();
        source.process(b"\x1b)");
        let mut restored =
            TerminalGridStream::from_snapshot(&source.snapshot(0, 2), limits).unwrap();
        for stream in [&mut source, &mut restored] {
            stream.process(b"0\x0eq\x1b7\x0fx\x1b8\x1b[2b\x0fq");
        }
        assert_eq!(source.snapshot(0, 2), restored.snapshot(0, 2));
        assert_eq!(
            crate::visible_text_lines(source.grid(), 0, 2)[0].trim_end(),
            "───q"
        );
        let delta = source.process_delta(b"\x0e").unwrap();
        restored.apply_delta(&delta, limits).unwrap();
        source.process(b"x");
        restored.process(b"x");
        assert_eq!(source.snapshot(0, 2), restored.snapshot(0, 2));
    }

    #[test]
    fn vertical_position_defaults_and_clamps_without_changing_column() {
        let mut grid = TerminalGrid::new(8, 4, GridLimits::default()).unwrap();
        grid.process(b"\x1b[3;4H\x1b[dA\x1b[0dB\x1b[999dC");
        let rows = grid.viewport_rows();
        assert_eq!(rows[0].cells()[3].text(), "A");
        assert_eq!(rows[0].cells()[4].text(), "B");
        assert_eq!(rows[3].cells()[5].text(), "C");
    }

    #[test]
    fn tui_ansi_frames_and_scroll_redraw_agree_with_grid_cells() {
        use bmux_tui::{buffer::Buffer, geometry::Rect, prelude::Line};
        let area = Rect::new(0, 0, 80, 3);
        let mut expected = Buffer::empty(area);
        expected.write_line(
            Rect::new(0, 0, 80, 1),
            &Line::raw("│ [界](https://example.com) > [👩‍💻](https://example.com) │"),
        );
        let blank = Buffer::empty(area);
        let mut stream = TerminalGridStream::new(80, 3, GridLimits::default()).unwrap();
        let mut bytes = Vec::new();
        bmux_tui::ansi::write_ansi_frame(&mut bytes, &expected, None).unwrap();
        for byte in &bytes {
            stream.process(&[*byte]);
        }
        for (before, after) in [(&expected, &blank), (&blank, &expected)] {
            bytes.clear();
            bmux_tui::ansi::write_ansi_frame_diff(&mut bytes, before, after, None).unwrap();
            for byte in &bytes {
                stream.process(&[*byte]);
            }
            let rows = stream.grid().viewport_rows();
            for (y, row) in rows.iter().enumerate() {
                let cells = row.visual_cells(80);
                for (x, cell) in cells.iter().enumerate() {
                    let expected_cell = after
                        .get(bmux_tui::geometry::Point::new(
                            u16::try_from(x).unwrap(),
                            u16::try_from(y).unwrap(),
                        ))
                        .unwrap();
                    assert_eq!(cell.text(), expected_cell.symbol, "at {x},{y}");
                    assert_eq!(
                        cell.is_wide_continuation(),
                        expected_cell.is_wide_continuation()
                    );
                }
            }
        }
    }

    #[test]
    fn streamed_graphemes_preserve_width_through_snapshot_and_redraw() {
        for cluster in ["👩‍💻", "👨‍👩‍👧‍👦", "🇺🇸", "👍🏽", "♥️", "界\u{301}"]
        {
            let input = format!("A{cluster}B");
            for split in 0..=input.len() {
                let limits = GridLimits::default();
                let mut stream = TerminalGridStream::new(12, 2, limits).unwrap();
                stream.process(&input.as_bytes()[..split]);
                let snapshot = stream.snapshot(0, 2);
                let mut restored = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
                stream.process(&input.as_bytes()[split..]);
                restored.process(&input.as_bytes()[split..]);
                assert_eq!(
                    stream.snapshot(0, 2),
                    restored.snapshot(0, 2),
                    "{cluster} split {split}"
                );
                let rows = stream.grid().viewport_rows();
                assert_eq!(rows[0].cells()[1].text(), cluster);
                assert_eq!(rows[0].cells()[1].width(), 2);
                assert!(rows[0].cells()[2].is_wide_continuation());
                assert_eq!(rows[0].cells()[3].text(), "B");
                assert_eq!(stream.grid().cursor().col, 4);
                stream.process(b"\r\x1b[2Kplain");
                assert_eq!(stream.grid().viewport_rows()[0].cells()[4].text(), "n");
            }
        }
    }

    #[test]
    fn joined_emoji_extends_at_pending_wrap_before_next_character_wraps() {
        let mut stream = TerminalGridStream::new(3, 2, GridLimits::default()).unwrap();
        stream.process("A👩‍💻B".as_bytes());
        let rows = stream.grid().viewport_rows();
        assert_eq!(rows[0].cells()[1].text(), "👩‍💻");
        assert_eq!(rows[1].cells()[0].text(), "B");
    }

    #[test]
    fn csi_cursor_position_moves_print_location() {
        let mut grid = TerminalGrid::new(10, 3, GridLimits::default()).unwrap();
        grid.process(b"\x1b[2;3HX");
        let rows = grid.viewport_rows();
        assert_eq!(rows[1].cells()[2].text(), "X");
    }

    #[test]
    fn erase_line_clears_content() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"abcdef\r\x1b[K");
        let rows = grid.viewport_rows();
        assert!(rows[0].cells().is_empty());
    }

    fn row_backgrounds(grid: &TerminalGrid, row: usize) -> Vec<Option<crate::style::Color>> {
        let rows = grid.viewport_rows();
        (0..grid.width())
            .map(|col| {
                rows[row]
                    .cells()
                    .get(col)
                    .and_then(|cell| grid.palette().get(cell.style()).bg)
            })
            .collect()
    }

    #[test]
    fn erase_line_applies_background_color_erase_to_full_width() {
        let mut grid = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"\x1b[41mabc\x1b[K");

        let red = Some(crate::style::Color::Indexed(1));
        assert_eq!(row_backgrounds(&grid, 0), vec![red; 10]);
    }

    #[test]
    fn erase_display_applies_background_color_erase_to_all_rows() {
        let mut grid = TerminalGrid::new(6, 3, GridLimits::default()).unwrap();
        grid.process(b"\x1b[44m\x1b[2J");

        let blue = Some(crate::style::Color::Indexed(4));
        for row in 0..3 {
            assert_eq!(row_backgrounds(&grid, row), vec![blue; 6]);
        }
    }

    #[test]
    fn erase_chars_applies_background_color_erase_to_requested_span() {
        let mut grid = TerminalGrid::new(8, 2, GridLimits::default()).unwrap();
        grid.process(b"\x1b[45m\x1b[3X");

        let magenta = Some(crate::style::Color::Indexed(5));
        assert_eq!(
            row_backgrounds(&grid, 0),
            vec![magenta, magenta, magenta, None, None, None, None, None]
        );
    }

    #[test]
    fn scroll_exposes_rows_with_background_color_erase() {
        let mut grid = TerminalGrid::new(4, 3, GridLimits::default()).unwrap();
        grid.process(b"\x1b[46m\x1b[L");

        let cyan = Some(crate::style::Color::Indexed(6));
        assert_eq!(row_backgrounds(&grid, 0), vec![cyan; 4]);
    }

    #[test]
    fn erase_with_default_background_stays_default_styled() {
        let mut grid = TerminalGrid::new(6, 2, GridLimits::default()).unwrap();
        // Bold/underline are glyph-only attributes and must not colorize erased
        // cells, so the row stays compact and default-styled.
        grid.process(b"\x1b[1;4mabc\x1b[2K");

        assert!(grid.viewport_rows()[0].cells().is_empty());
    }

    #[test]
    fn terminal_reset_preserves_revision_order_and_retention_limits() {
        let limits = GridLimits { scrollback_rows: 2 };
        let mut producer = TerminalGridStream::new(10, 3, limits).unwrap();
        producer.process(b"main\x1b[?1049h\x1b[31malt");
        let before = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        let delta = producer.process_delta(b"\x1bc").unwrap();
        assert!(delta.revision > before.revision);
        assert!(delta.content_revision > before.content_revision);
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        assert_eq!(producer.grid().mode(), crate::GridMode::Main);
        assert!(producer.snapshot(0, 3).main_rows.is_none());
        for _ in 0..20 {
            producer.process(b"line\r\n");
        }
        assert!(producer.grid().max_scrollback_offset() <= limits.scrollback_rows);
        let before = producer.snapshot(0, 3);
        let delta = producer.process_delta(b"\x1bc").unwrap();
        assert!(delta.revision > before.revision);
        assert_eq!(producer.grid().max_scrollback_offset(), 0);
    }

    #[test]
    fn failed_delta_hydration_preserves_grid_and_parser_for_retry() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
        producer.process(b"main\x1b[?1049halt\x1b[");
        let before = producer.snapshot(0, 3);
        let delta = producer.process_delta(b"31mred").unwrap();
        let after = producer.snapshot(0, 3);
        for invalid in 0..15 {
            let mut malformed = delta.clone();
            match invalid {
                0 => malformed.width = 0,
                1 => malformed.height = 0,
                2 => malformed.mode = "invalid".to_string(),
                3 => malformed.content_revision = before.content_revision - 1,
                4 => malformed.base_revision = before.revision + 1,
                5 => malformed.revision = before.revision,
                6 => malformed.revision = before.revision - 1,
                7 => {
                    assert!(!malformed.reset_rows);
                    malformed.scrollback_rows += 1;
                }
                8 => {
                    assert!(!malformed.reset_rows);
                    malformed.row_updates[0].row_index = u32::MAX;
                }
                9 => {
                    assert!(!malformed.reset_rows);
                    malformed.row_updates.push(malformed.row_updates[0].clone());
                }
                10 => malformed.width += 1,
                11 => malformed.height += 1,
                12 => malformed.mode = "main".to_string(),
                13 => malformed.total_scrolled_rows = Some(before.total_scrolled_rows.unwrap() + 1),
                _ => {
                    malformed.reset_rows = true;
                    assert!(!malformed.row_updates.is_empty());
                    malformed.row_updates[0].row_index = 1;
                }
            }
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            assert!(consumer.apply_delta(&malformed, limits).is_err());
            assert_eq!(consumer.snapshot(0, 3), before);
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), after);

            // The live parser, not just its serialized prefix, must survive a
            // rejected hydration so subsequent bytes complete the same sequence.
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            assert!(consumer.apply_delta(&malformed, limits).is_err());
            consumer.process(b"31mred");
            assert_eq!(consumer.snapshot(0, 3), after);
            consumer.process(b"\x1b[?1049l!");
            let mut expected = TerminalGridStream::from_snapshot(&after, limits).unwrap();
            expected.process(b"\x1b[?1049l!");
            assert_eq!(consumer.snapshot(0, 3), expected.snapshot(0, 3));
        }
    }

    #[test]
    fn omitted_hidden_backing_obeys_receiver_retention() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour\x1b[?1049halt");
        let before = producer.snapshot(0, producer.grid().main_row_count());
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"!");
        let after = producer.snapshot(0, producer.grid().main_row_count());
        let delta = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(delta.main_rows.is_none());
        let history = before.main_rows.as_ref().unwrap().len() - usize::from(before.height);
        assert!(history > 0);
        assert!(matches!(
            replica.apply_delta(
                &delta,
                GridLimits {
                    scrollback_rows: history - 1
                }
            ),
            Err(crate::TerminalGridStreamDeltaError::IncompleteHistory { .. })
        ));
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), before);
        replica
            .apply_delta(
                &delta,
                GridLimits {
                    scrollback_rows: history,
                },
            )
            .unwrap();
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), after);
    }

    #[test]
    fn zero_history_limit_replicates_split_wide_text() {
        let limits = GridLimits { scrollback_rows: 0 };
        let input = "\x1b[31m界界界界界e\u{301}\x1b[?1049halt\x1b[?1049l界".as_bytes();
        for split in 0..=input.len() {
            let mut producer = TerminalGridStream::new(3, 2, limits).unwrap();
            let mut replica = TerminalGridStream::new(3, 2, limits).unwrap();
            let mut raw = TerminalGridStream::new(3, 2, limits).unwrap();
            for (bytes, width) in [(&input[..split], 2), (&input[split..], 7)] {
                raw.process(bytes);
                if let Some(delta) = producer.process_delta(bytes) {
                    replica.apply_delta(&delta, limits).unwrap();
                }
                assert_eq!(
                    replica.snapshot(0, replica.grid().main_row_count()),
                    producer.snapshot(0, producer.grid().main_row_count()),
                    "split {split}"
                );
                assert_eq!(
                    producer.snapshot(0, producer.grid().main_row_count()),
                    raw.snapshot(0, raw.grid().main_row_count())
                );
                raw.resize(width, 2).unwrap();
                let delta = producer.resize_delta(width, 2).unwrap().unwrap();
                replica.apply_delta(&delta, limits).unwrap();
                assert_eq!(
                    replica.snapshot(0, replica.grid().main_row_count()),
                    producer.snapshot(0, producer.grid().main_row_count()),
                    "split {split}, width {width}"
                );
                assert_eq!(
                    producer.snapshot(0, producer.grid().main_row_count()),
                    raw.snapshot(0, raw.grid().main_row_count())
                );
            }
        }
    }

    #[test]
    fn zero_history_limit_preserves_unfinished_wrapped_line() {
        let limits = GridLimits { scrollback_rows: 0 };
        let mut producer = TerminalGridStream::new(3, 2, limits).unwrap();
        let mut replica = TerminalGridStream::new(3, 2, limits).unwrap();
        for bytes in [
            b"abcdefghijkl".as_slice(),
            b"m",
            b"\x1b[?1049halt",
            b"!",
            b"\x1b[?1049l",
        ] {
            let delta = producer.process_delta(bytes).unwrap();
            replica.apply_delta(&delta, limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
        assert!(producer.grid().scrollback_rows_hint() > 0);
        assert_eq!(producer.grid().retained_history_line_count(), 0);
        for width in [2, 12] {
            let delta = producer.resize_delta(width, 2).unwrap().unwrap();
            replica.apply_delta(&delta, limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
        let delta = producer.process_delta(b"\r\nnext\r\nlast").unwrap();
        replica.apply_delta(&delta, limits).unwrap();
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
        assert_eq!(producer.grid().scrollback_rows_hint(), 0);
    }

    #[test]
    fn wrapped_hidden_history_uses_logical_retention_limit() {
        let limits = GridLimits { scrollback_rows: 2 };
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"first-line\r\nsecond-line\r\nthird\r\nfourth");
        producer.resize(3, 2).unwrap();
        assert!(producer.grid().scrollback_rows_hint() > limits.scrollback_rows);
        let before = producer.snapshot(0, producer.grid().main_row_count());
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        for bytes in [b"\x1b[?1049halt".as_slice(), b"!", b"\x1b[?1049l"] {
            let delta = producer.process_delta(bytes).unwrap();
            if bytes == b"!" {
                assert!(delta.main_rows.is_none());
            }
            let unchanged = replica.snapshot(0, replica.grid().main_row_count());
            assert!(matches!(
                replica.apply_delta(&delta, GridLimits { scrollback_rows: 0 }),
                Err(crate::TerminalGridStreamDeltaError::IncompleteHistory { .. })
            ));
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                unchanged
            );
            replica.apply_delta(&delta, limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
    }

    #[test]
    fn hidden_replacement_exceeding_retention_preserves_replica() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        let before = producer.snapshot(0, 2);
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour\x1b[?1049halt");
        let after = producer.snapshot(0, producer.grid().main_row_count());
        assert_eq!(after.scrollback_rows, 0);
        assert!(after.main_rows.as_ref().unwrap().len() > 3);
        let delta = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(
            replica
                .apply_delta(&delta, GridLimits { scrollback_rows: 1 })
                .is_err()
        );
        assert_eq!(replica.snapshot(0, 2), before);
        let exact_limits = GridLimits {
            scrollback_rows: after.main_rows.as_ref().unwrap().len() - usize::from(after.height),
        };
        replica.apply_delta(&delta, exact_limits).unwrap();
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), after);
        producer.process(b"\x1b[?1049l");
        replica.process(b"\x1b[?1049l");
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
    }

    #[test]
    fn replacement_exceeding_receiver_retention_leaves_replica_unchanged() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        let before = producer.snapshot(0, 2);
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour");
        let after = producer.snapshot(0, producer.grid().main_row_count());
        let delta = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(
            replica
                .apply_delta(&delta, GridLimits { scrollback_rows: 1 })
                .is_err()
        );
        assert_eq!(replica.snapshot(0, 2), before);
        let exact_limits = GridLimits {
            scrollback_rows: usize::try_from(after.scrollback_rows).unwrap(),
        };
        replica.apply_delta(&delta, exact_limits).unwrap();
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), after);

        // Successful hydration must install the receiver's limit, not retain
        // the larger limit with which the replica was originally constructed.
        let mut expected = TerminalGridStream::from_snapshot(&after, exact_limits).unwrap();
        replica.process(b"\r\nfive");
        expected.process(b"\r\nfive");
        let actual = replica.snapshot(0, replica.grid().main_row_count());
        assert_eq!(actual.scrollback_rows, after.scrollback_rows);
        assert_eq!(
            actual,
            expected.snapshot(0, expected.grid().main_row_count())
        );
        for bytes in [b"\r\nsix".as_slice(), b"\r\nseven", b"!"] {
            let delta = expected.process_delta(bytes).unwrap();
            replica.apply_delta(&delta, exact_limits).unwrap();
            let actual = replica.snapshot(0, replica.grid().main_row_count());
            assert_eq!(actual.scrollback_rows, after.scrollback_rows);
            assert_eq!(
                actual,
                expected.snapshot(0, expected.grid().main_row_count())
            );
        }
        for (width, height) in [(3, 2), (12, 4), (12, 2)] {
            let delta = expected.resize_delta(width, height).unwrap().unwrap();
            replica.apply_delta(&delta, exact_limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                expected.snapshot(0, expected.grid().main_row_count())
            );
            let delta = expected.process_delta(b"\r\nnext").unwrap();
            replica.apply_delta(&delta, exact_limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                expected.snapshot(0, expected.grid().main_row_count())
            );
        }
    }

    #[test]
    fn understated_replacement_history_obeys_receiver_retention() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        let before = producer.snapshot(0, 2);
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour");
        let after = producer.snapshot(0, producer.grid().main_row_count());
        let valid = crate::GridDeltaBatch::between(&before, &after).unwrap();
        let mut understated = valid.clone();
        understated.scrollback_rows = 1;
        assert!(matches!(
            replica.apply_delta(&understated, GridLimits { scrollback_rows: 1 }),
            Err(crate::TerminalGridStreamDeltaError::IncompleteHistory { .. })
        ));
        assert_eq!(replica.snapshot(0, 2), before);
        assert!(matches!(
            replica.apply_delta(&understated, limits),
            Err(crate::TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: 1,
                reconstructed_rows: 2,
            })
        ));
        assert_eq!(replica.snapshot(0, 2), before);
        replica.apply_delta(&valid, limits).unwrap();
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), after);
    }

    #[test]
    fn complete_scrolling_replacement_then_sparse_update_preserves_history() {
        let limits = GridLimits { scrollback_rows: 2 };
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour");
        let before = producer.snapshot(0, producer.grid().main_row_count());
        let mut replica = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"\r\nfive");
        let after = producer.snapshot(0, producer.grid().main_row_count());
        let replacement = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(replacement.reset_rows);
        replica.apply_delta(&replacement, limits).unwrap();
        assert_eq!(replica.snapshot(0, replica.grid().main_row_count()), after);
        let sparse = producer.process_delta(b"\x1b[1;1Hupdated").unwrap();
        assert!(!sparse.reset_rows);
        replica.apply_delta(&sparse, limits).unwrap();
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
    }

    #[test]
    fn sparse_viewport_updates_preserve_wrapped_styled_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(8, 3, limits).unwrap();
        producer.process(
            "\x1b[31mhistory界abcdefgh\x1b[0m\r\nsecond\r\nthird\r\nfourth\r\nfifth".as_bytes(),
        );
        let baseline = producer.snapshot(0, producer.grid().main_row_count());
        let history_len = baseline.rows.len() - 3;
        assert!(history_len > 0);
        assert!(baseline.rows[..history_len].iter().any(|row| row.wrapped));
        let mut replica = TerminalGridStream::from_snapshot(&baseline, limits).unwrap();
        for bytes in [
            b"\x1b[1;1H\x1b[32mTOP".as_slice(),
            b"\x1b[3;1H\x1b[34mBOTTOM".as_slice(),
        ] {
            let delta = producer.process_delta(bytes).unwrap();
            assert!(!delta.reset_rows);
            replica.apply_delta(&delta, limits).unwrap();
            let actual = replica.snapshot(0, replica.grid().main_row_count());
            assert_eq!(&actual.rows[..history_len], &baseline.rows[..history_len]);
            assert_eq!(
                actual,
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
    }

    #[test]
    fn alternate_resize_deltas_preserve_backing_through_exit() {
        let limits = GridLimits {
            scrollback_rows: 20,
        };
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"first line\r\nsecond line\r\nthird line\r\nfourth line\x1b[?1049halt");
        let baseline = producer.snapshot(0, producer.grid().main_row_count());
        let mut replica = TerminalGridStream::from_snapshot(&baseline, limits).unwrap();
        for (width, height) in [(6, 2), (16, 3), (12, 1), (12, 2)] {
            let delta = producer.resize_delta(width, height).unwrap().unwrap();
            replica.apply_delta(&delta, limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
        let exit = producer.process_delta(b"\x1b[?1049l").unwrap();
        let after_exit = producer.snapshot(0, producer.grid().main_row_count());
        replica.apply_delta(&exit, limits).unwrap();
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            after_exit
        );
        let update = producer.process_delta(b"!").unwrap();
        replica.apply_delta(&update, limits).unwrap();
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
    }

    #[test]
    fn resize_delta_with_history_applies_without_snapshot_recovery() {
        let limits = GridLimits::default();
        for (width, height) in [(6, 2), (16, 3), (12, 1)] {
            let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
            producer.process(b"first line\r\nsecond line\r\nthird line\r\nfourth line");
            let baseline = producer.snapshot(0, producer.grid().main_row_count());
            let mut replica = TerminalGridStream::from_snapshot(&baseline, limits).unwrap();
            let delta = producer.resize_delta(width, height).unwrap().unwrap();
            replica.apply_delta(&delta, limits).unwrap();
            let complete = producer.snapshot(0, producer.grid().main_row_count());
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                complete
            );
        }
    }

    #[test]
    fn scrolling_at_retention_limit_applies_without_recovery() {
        let limits = GridLimits { scrollback_rows: 2 };
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree\r\nfour");
        let baseline = producer.snapshot(0, producer.grid().main_row_count());
        assert_eq!(baseline.scrollback_rows, 2);
        let mut replica = TerminalGridStream::from_snapshot(&baseline, limits).unwrap();
        let delta = producer.process_delta(b"\r\nfive").unwrap();
        assert_eq!(delta.scrollback_rows, baseline.scrollback_rows);
        assert_ne!(delta.total_scrolled_rows, baseline.total_scrolled_rows);
        assert!(delta.reset_rows);
        replica.apply_delta(&delta, limits).unwrap();
        let complete = producer.snapshot(0, producer.grid().main_row_count());
        assert_ne!(complete.rows, baseline.rows);
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            complete
        );
    }

    #[test]
    fn every_split_replicates_scrolling_and_screen_transitions() {
        let limits = GridLimits { scrollback_rows: 3 };
        let bytes =
            "first\r\n\x1b[31m界é\x1b[0m\r\nthird\r\nfourth\r\nfifth\x1b[?1049halt\x1b[?1049l!"
                .as_bytes();
        let mut whole = TerminalGridStream::new(8, 2, limits).unwrap();
        whole.process(bytes);
        let expected = whole.snapshot(0, whole.grid().main_row_count());
        for split in 0..=bytes.len() {
            let mut producer = TerminalGridStream::new(8, 2, limits).unwrap();
            let mut replica = TerminalGridStream::new(8, 2, limits).unwrap();
            for chunk in [&bytes[..split], &bytes[split..]] {
                if let Some(delta) = producer.process_delta(chunk) {
                    replica.apply_delta(&delta, limits).unwrap();
                }
                assert_eq!(
                    replica.snapshot(0, replica.grid().main_row_count()),
                    producer.snapshot(0, producer.grid().main_row_count()),
                    "split {split}"
                );
            }
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                expected
            );
        }
        for chunk_size in 1..=bytes.len() {
            let mut producer = TerminalGridStream::new(8, 2, limits).unwrap();
            let mut replica = TerminalGridStream::new(8, 2, limits).unwrap();
            for chunk in bytes.chunks(chunk_size) {
                if let Some(delta) = producer.process_delta(chunk) {
                    let encoded = serde_json::to_vec(&delta).unwrap();
                    let decoded = serde_json::from_slice(&encoded).unwrap();
                    replica.apply_delta(&decoded, limits).unwrap();
                    let accepted = replica.snapshot(0, replica.grid().main_row_count());
                    assert!(matches!(
                        replica.apply_delta(&decoded, limits),
                        Err(super::TerminalGridStreamDeltaError::Delta(
                            crate::GridDeltaApplyError::RevisionMismatch { .. }
                        ))
                    ));
                    assert_eq!(
                        replica.snapshot(0, replica.grid().main_row_count()),
                        accepted
                    );
                }
                assert_eq!(
                    replica.snapshot(0, replica.grid().main_row_count()),
                    producer.snapshot(0, producer.grid().main_row_count()),
                    "chunk size {chunk_size}"
                );
            }
            // Prefix-only chunks advance replication without changing content.
            // Compare presentation with the unsplit stream independently of that
            // chunk-dependent replication counter; per-chunk checks above are exact.
            let mut actual = replica.snapshot(0, replica.grid().main_row_count());
            actual.revision = expected.revision;
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn missing_update_recovers_pending_sequence_from_wire_snapshot() {
        let limits = GridLimits { scrollback_rows: 3 };
        let mut producer = TerminalGridStream::new(8, 2, limits).unwrap();
        let mut replica = TerminalGridStream::new(8, 2, limits).unwrap();
        let initial = replica.snapshot(0, 2);
        let missing = producer.process_delta(b"one\r\ntwo\r\nthree").unwrap();
        let later = producer.process_delta(b"\x1b[?1049halt\x1b[31").unwrap();
        assert!(replica.apply_delta(&later, limits).is_err());
        assert_eq!(replica.snapshot(0, 2), initial);
        let resize = producer.resize_delta(5, 3).unwrap().unwrap();
        assert!(replica.apply_delta(&resize, limits).is_err());
        assert_eq!(replica.snapshot(0, 2), initial);
        let mut replay = TerminalGridStream::from_snapshot(&initial, limits).unwrap();
        for delta in [&missing, &later, &resize] {
            replay.apply_delta(delta, limits).unwrap();
        }
        assert_eq!(
            replay.snapshot(0, replay.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
        let recovery = producer.snapshot(0, producer.grid().main_row_count());
        assert!(!recovery.pending_bytes.is_empty());
        let wire = serde_json::to_vec(&recovery).unwrap();
        let decoded = serde_json::from_slice(&wire).unwrap();
        replica = TerminalGridStream::from_snapshot(&decoded, limits).unwrap();
        assert!(replica.apply_delta(&missing, limits).is_err());
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            recovery
        );
        for bytes in [b"mred".as_slice(), b"\x1b[?1049l", b"\r\nfour"] {
            let delta = producer.process_delta(bytes).unwrap();
            replica.apply_delta(&delta, limits).unwrap();
            assert_eq!(
                replica.snapshot(0, replica.grid().main_row_count()),
                producer.snapshot(0, producer.grid().main_row_count())
            );
        }
    }

    #[test]
    fn scrolling_delta_then_sparse_update_preserves_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(12, 2, limits).unwrap();
        producer.process(b"first\r\nsecond");
        let baseline = producer.snapshot(0, 2);
        let mut replica = TerminalGridStream::from_snapshot(&baseline, limits).unwrap();
        let delta = producer.process_delta(b"\r\nthird").unwrap();
        replica.apply_delta(&delta, limits).unwrap();
        let complete = producer.snapshot(0, producer.grid().main_row_count());
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            complete
        );
        let delta = producer.process_delta(b"!").unwrap();
        replica.apply_delta(&delta, limits).unwrap();
        assert_eq!(
            replica.snapshot(0, replica.grid().main_row_count()),
            producer.snapshot(0, producer.grid().main_row_count())
        );
    }

    #[test]
    fn every_split_preserves_mixed_stream_state_after_hydration() {
        let bytes = "main\x1b[?1049h\x1b[31m界é\x1b[0m\x1b[?1049l!".as_bytes();
        let limits = GridLimits::default();
        let mut whole = TerminalGridStream::new(20, 3, limits).unwrap();
        whole.process(bytes);
        let expected = whole.snapshot(0, 3);
        for split in 0..=bytes.len() {
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            let mut replica =
                TerminalGridStream::from_snapshot(&producer.snapshot(0, 3), limits).unwrap();
            if let Some(delta) = producer.process_delta(&bytes[..split]) {
                replica.apply_delta(&delta, limits).unwrap();
            }
            assert_eq!(
                replica.snapshot(0, 3),
                producer.snapshot(0, 3),
                "prefix {split}"
            );
            let mut consumer =
                TerminalGridStream::from_snapshot(&producer.snapshot(0, 3), limits).unwrap();
            if let Some(delta) = producer.process_delta(&bytes[split..]) {
                replica.apply_delta(&delta, limits).unwrap();
            }
            assert_eq!(
                replica.snapshot(0, 3),
                producer.snapshot(0, 3),
                "suffix {split}"
            );
            consumer.process(&bytes[split..]);
            let mut actual = consumer.snapshot(0, 3);
            assert_eq!(actual, producer.snapshot(0, 3), "split {split}");
            // Prefix-only chunks legitimately add replication revisions.
            actual.revision = expected.revision;
            assert_eq!(actual, expected, "split {split}");
        }
    }

    #[test]
    fn utf8_boundary_prefixes_match_stream_hydration() {
        let limits = GridLimits::default();
        for (prefix, suffix) in [
            (&b"\xc2"[..], &b"\x80"[..]),
            (&b"\xe0\xa0"[..], &b"\x80"[..]),
            (&b"\xed\x9f"[..], &b"\xbf"[..]),
            (&b"\xf0\x90\x80"[..], &b"\x80"[..]),
            (&b"\xf4\x8f\xbf"[..], &b"\xbf"[..]),
        ] {
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            producer.process(prefix);
            let snapshot = producer.snapshot(0, 3);
            assert_eq!(snapshot.pending_bytes, prefix);
            let mut consumer = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
            producer.process(suffix);
            consumer.process(suffix);
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            assert!(producer.snapshot(0, 3).pending_bytes.is_empty());
        }
        for invalid in [
            &b"\xc0"[..],
            &b"\xc1"[..],
            &b"\xe0\x9f"[..],
            &b"\xed\xa0"[..],
            &b"\xf0\x8f"[..],
            &b"\xf4\x90"[..],
            &b"\xf5"[..],
        ] {
            assert!(super::trailing_incomplete_sequence(invalid).is_empty());
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            producer.process(invalid);
            let snapshot = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
            for suffix in [&b"\x80"[..], &b"ok\x1b[31mred"[..]] {
                producer.process(suffix);
                consumer.process(suffix);
                assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            }
        }
    }

    #[test]
    fn malformed_utf8_tails_do_not_retain_false_prefixes() {
        for leading in [0xc2, 0xe7, 0xf0, 0xff] {
            for count in [4, 8, 4096] {
                let mut bytes = vec![leading];
                bytes.extend(std::iter::repeat_n(0x80, count));
                assert!(super::trailing_incomplete_sequence(&bytes).is_empty());
                bytes.extend_from_slice(b"\xf0\x9f\x98");
                assert_eq!(super::trailing_incomplete_sequence(&bytes), b"\xf0\x9f\x98");
            }
        }
    }

    #[test]
    fn invalid_utf8_before_split_character_preserves_continuity() {
        let limits = GridLimits::default();
        for invalid in [&b"\xfftext"[..], &b"\x80\x80"[..], &b"\xc0\xaf"[..]] {
            for character in ["é", "界", "😀"] {
                let bytes = character.as_bytes();
                let mut complete = invalid.to_vec();
                complete.extend_from_slice(bytes);
                let mut uninterrupted = TerminalGridStream::new(20, 3, limits).unwrap();
                uninterrupted.process(&complete);
                let expected = uninterrupted.snapshot(0, 3);
                for split in 1..bytes.len() {
                    let mut prefix = invalid.to_vec();
                    prefix.extend_from_slice(&bytes[..split]);
                    let suffix = &bytes[split..];
                    let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
                    let mut replica = TerminalGridStream::new(20, 3, limits).unwrap();
                    let delta = producer.process_delta(&prefix).unwrap();
                    assert_eq!(delta.pending_bytes, bytes[..split]);
                    replica.apply_delta(&delta, limits).unwrap();
                    let mut hydrated =
                        TerminalGridStream::from_snapshot(&producer.snapshot(0, 3), limits)
                            .unwrap();
                    let delta = producer.process_delta(suffix).unwrap();
                    replica.apply_delta(&delta, limits).unwrap();
                    hydrated.process(suffix);
                    let mut actual = producer.snapshot(0, 3);
                    assert_eq!(replica.snapshot(0, 3), actual);
                    assert_eq!(hydrated.snapshot(0, 3), actual);
                    // Chunk boundaries may add parser-prefix revisions only.
                    actual.revision = expected.revision;
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn reordered_stream_deltas_preserve_pending_parser_and_allow_recovery() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
        producer.process(b"main\x1b[?1049halt");
        let before = producer.snapshot(0, 3);
        let first = producer.process_delta(b"\x1b[").unwrap();
        let first: crate::GridDeltaBatch =
            serde_json::from_slice(&serde_json::to_vec(&first).unwrap()).unwrap();
        let middle = producer.snapshot(0, 3);
        let second = producer.process_delta(b"31mred").unwrap();
        let second: crate::GridDeltaBatch =
            serde_json::from_slice(&serde_json::to_vec(&second).unwrap()).unwrap();
        let after = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        assert!(consumer.apply_delta(&second, limits).is_err());
        assert_eq!(consumer.snapshot(0, 3), before);
        consumer.apply_delta(&first, limits).unwrap();
        assert!(consumer.apply_delta(&first, limits).is_err());
        assert_eq!(consumer.snapshot(0, 3), middle);
        let mut malformed = second.clone();
        malformed.width = 0;
        assert!(matches!(
            consumer.apply_delta(&malformed, limits),
            Err(super::TerminalGridStreamDeltaError::Delta(
                crate::GridDeltaApplyError::ZeroDimensions
            ))
        ));
        assert_eq!(consumer.snapshot(0, 3), middle);
        let mut invalid_mode = second.clone();
        invalid_mode.mode = "unknown".to_owned();
        assert!(matches!(
            consumer.apply_delta(&invalid_mode, limits),
            Err(super::TerminalGridStreamDeltaError::Delta(
                crate::GridDeltaApplyError::InvalidScreenMode
            ))
        ));
        assert_eq!(consumer.snapshot(0, 3), middle);
        consumer.apply_delta(&second, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), after);

        // Recover through raw continuation instead of applying the second delta.
        // This exercises the live parser prefix after rejecting both a duplicate
        // and a malformed update, rather than replacing it from a valid delta.
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        consumer.apply_delta(&first, limits).unwrap();
        assert!(consumer.apply_delta(&first, limits).is_err());
        assert!(consumer.apply_delta(&malformed, limits).is_err());
        consumer.process(b"31mred");
        assert_eq!(consumer.snapshot(0, 3), after);
        producer.process(b"\x1b[?1049l!");
        consumer.process(b"\x1b[?1049l!");
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn resize_delta_preserves_split_stream_continuity() {
        let bytes = "main\x1b[?1049h\x1b[31m界é\x1b[0m\x1b[?1049l!".as_bytes();
        let limits = GridLimits::default();
        for split in 0..=bytes.len() {
            for (width, height) in [(10, 2), (30, 5)] {
                let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
                producer.process(&bytes[..split]);
                let mut replica =
                    TerminalGridStream::from_snapshot(&producer.snapshot(0, 3), limits).unwrap();
                let delta = producer.resize_delta(width, height).unwrap().unwrap();
                replica.apply_delta(&delta, limits).unwrap();
                assert_eq!(
                    replica.snapshot(0, usize::from(height)),
                    producer.snapshot(0, usize::from(height)),
                    "resize at {split}"
                );
                let mut structured = TerminalGridStream::from_snapshot(
                    &replica.snapshot(0, usize::from(height)),
                    limits,
                )
                .unwrap();
                // Both live parsers must resume from the same incomplete sequence.
                if let Some(delta) = producer.process_delta(&bytes[split..]) {
                    structured.apply_delta(&delta, limits).unwrap();
                }
                assert_eq!(
                    structured.snapshot(0, usize::from(height)),
                    producer.snapshot(0, usize::from(height)),
                    "output delta at {split}"
                );
                replica.process(&bytes[split..]);
                assert_eq!(
                    replica.snapshot(0, usize::from(height)),
                    producer.snapshot(0, usize::from(height)),
                    "continuation at {split}"
                );
            }
        }
    }

    #[test]
    fn empty_protocol_input_preserves_split_alternate_transition() {
        let mut tracker = super::TerminalProtocolTracker::new();
        assert!(!tracker.process(b"\x1b[?1049").toggled_alternate);
        let protocol = tracker.protocol_state();
        assert!(!tracker.process(b"").toggled_alternate);
        assert_eq!(tracker.protocol_state(), protocol);
        assert!(!tracker.alternate_screen());
        assert!(tracker.process(b"h").toggled_alternate);
        assert!(tracker.alternate_screen());
        assert!(!tracker.process(b"").toggled_alternate);
        assert!(tracker.alternate_screen());
        assert!(tracker.process(b"\x1b[?1049l").toggled_alternate);
        assert!(!tracker.alternate_screen());
    }

    #[test]
    fn incomplete_main_backing_rejects_atomically_on_entry_and_sparse_updates() {
        for already_alternate in [false, true] {
            let limits = GridLimits::default();
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            producer.process(b"retained main");
            if already_alternate {
                producer.process(b"\x1b[?1049halt");
            }
            let before = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            if already_alternate {
                producer.process(b" update");
            } else {
                producer.process(b"\x1b[?1049halt");
            }
            let after = producer.snapshot(0, 3);
            let valid = crate::GridDeltaBatch::between(&before, &after).unwrap();
            assert_eq!(valid.reset_rows, !already_alternate);
            for count in [0, 2] {
                let mut malformed = valid.clone();
                malformed.main_rows = after.main_rows.clone();
                malformed.main_rows.as_mut().unwrap().truncate(count);
                let mut snapshot = before.clone();
                assert!(matches!(
                    malformed.apply_to_snapshot(&mut snapshot),
                    Err(crate::GridDeltaApplyError::IncompleteMainViewport { .. })
                ));
                assert_eq!(snapshot, before);
                assert!(matches!(
                    consumer.apply_delta(&malformed, limits),
                    Err(super::TerminalGridStreamDeltaError::Delta(
                        crate::GridDeltaApplyError::IncompleteMainViewport { .. }
                    ))
                ));
                assert_eq!(consumer.snapshot(0, 3), before);
            }
            consumer.apply_delta(&valid, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), after);
            producer.process(b"\x1b[?1049l continued");
            consumer.process(b"\x1b[?1049l continued");
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn incomplete_replacement_viewport_preserves_state_and_allows_retry() {
        for alternate in [false, true] {
            let limits = GridLimits::default();
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            producer.process(b"retained main\x1b[");
            let before = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            producer.process(b"31m");
            if alternate {
                producer.process(b"\x1b[?1049halt");
            } else {
                producer.resize(21, 3).unwrap();
            }
            let after = producer.snapshot(0, 3);
            let valid = crate::GridDeltaBatch::between(&before, &after).unwrap();
            assert!(valid.reset_rows);
            for rows in [0, 2] {
                let mut malformed = valid.clone();
                malformed.row_updates.truncate(rows);
                let mut snapshot = before.clone();
                assert!(matches!(
                    malformed.apply_to_snapshot(&mut snapshot),
                    Err(crate::GridDeltaApplyError::IncompleteViewport { .. })
                ));
                assert_eq!(snapshot, before);
                assert!(matches!(
                    consumer.apply_delta(&malformed, limits),
                    Err(super::TerminalGridStreamDeltaError::Delta(
                        crate::GridDeltaApplyError::IncompleteViewport { .. }
                    ))
                ));
                assert_eq!(consumer.snapshot(0, 3), before);
            }
            consumer.apply_delta(&valid, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), after);
            producer.process(b"\x1b[?1049lcontinued");
            consumer.process(b"\x1b[?1049lcontinued");
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn invalid_active_cursor_rejects_without_losing_pending_wrap() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(10, 2, limits).unwrap();
        producer.process(b"123456789");
        let before = producer.snapshot(0, 2);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        let valid = producer.process_delta(b"0").unwrap();
        assert!(valid.pending_wrap);
        assert_eq!(valid.cursor.col, 9);
        for (row, col) in [(2, 0), (0, 10), (u16::MAX, u16::MAX)] {
            let mut malformed = valid.clone();
            malformed.cursor.row = row;
            malformed.cursor.col = col;
            let mut snapshot = before.clone();
            assert!(matches!(
                malformed.apply_to_snapshot(&mut snapshot),
                Err(crate::GridDeltaApplyError::InvalidCursor)
            ));
            assert_eq!(snapshot, before);
            assert!(matches!(
                consumer.apply_delta(&malformed, limits),
                Err(super::TerminalGridStreamDeltaError::Delta(
                    crate::GridDeltaApplyError::InvalidCursor
                ))
            ));
            assert_eq!(consumer.snapshot(0, 2), before);
        }
        consumer.apply_delta(&valid, limits).unwrap();
        producer.process(b"next");
        consumer.process(b"next");
        assert_eq!(consumer.snapshot(0, 2), producer.snapshot(0, 2));
    }

    #[test]
    fn malformed_scroll_regions_preserve_state_and_allow_retry() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 4, limits).unwrap();
        producer.process(b"first\r\nsecond\r\nthird\r\nfourth");
        let before = producer.snapshot(0, 4);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        let valid = producer.process_delta(b"\x1b[2;3r").unwrap();
        for (top, bottom) in [(1, 1), (2, 1), (0, 4), (4, 5)] {
            let mut malformed = valid.clone();
            malformed.scroll_region = Some(crate::ScrollRegionSnapshot { top, bottom });
            let mut snapshot = before.clone();
            assert!(matches!(
                malformed.apply_to_snapshot(&mut snapshot),
                Err(crate::GridDeltaApplyError::InvalidScrollRegion)
            ));
            assert_eq!(snapshot, before);
            assert!(matches!(
                consumer.apply_delta(&malformed, limits),
                Err(super::TerminalGridStreamDeltaError::Delta(
                    crate::GridDeltaApplyError::InvalidScrollRegion
                ))
            ));
            assert_eq!(consumer.snapshot(0, 4), before);
        }
        consumer.apply_delta(&valid, limits).unwrap();
        for bytes in [b"\x1b[3;1H\ninside".as_slice(), b"\x1b[r"] {
            producer.process(bytes);
            consumer.process(bytes);
            assert_eq!(consumer.snapshot(0, 4), producer.snapshot(0, 4));
        }
    }

    #[test]
    fn excess_alternate_replacement_rows_reject_atomically() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"retained main");
        let before = producer.snapshot(0, 2);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        let valid = producer.process_delta(b"\x1b[?1049halt").unwrap();
        let mut malformed = valid.clone();
        let mut extra = malformed.row_updates[0].clone();
        extra.row_index = 2;
        malformed.row_updates.push(extra);
        let mut snapshot = before.clone();
        assert!(matches!(
            malformed.apply_to_snapshot(&mut snapshot),
            Err(crate::GridDeltaApplyError::ExcessAlternateRows {
                expected: 2,
                actual: 3,
            })
        ));
        assert_eq!(snapshot, before);
        assert!(matches!(
            consumer.apply_delta(&malformed, limits),
            Err(super::TerminalGridStreamDeltaError::Delta(
                crate::GridDeltaApplyError::ExcessAlternateRows { .. }
            ))
        ));
        assert_eq!(consumer.snapshot(0, 2), before);
        consumer.apply_delta(&valid, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 2), producer.snapshot(0, 2));
        producer.process(b"\x1b[?1049l continued");
        consumer.process(b"\x1b[?1049l continued");
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn malformed_alternate_delta_does_not_project_retained_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        for _ in 0..500 {
            producer.process(b"retained\r\n");
        }
        producer.process(b"\x1b[?1049halt");
        let before = producer.snapshot(0, 502);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        let valid = producer.process_delta(b"!").unwrap();
        for duplicate in [false, true] {
            let mut malformed = valid.clone();
            if duplicate {
                malformed.row_updates.push(malformed.row_updates[0].clone());
            } else {
                malformed.row_updates[0].row_index = 2;
            }
            crate::reflow::reset_projection_stats();
            assert!(consumer.apply_delta(&malformed, limits).is_err());
            let stats = crate::reflow::projection_stats();
            assert!(
                stats.logical_lines_projected <= 2,
                "invalid viewport update projected hidden history: {stats:?}"
            );
            assert_eq!(consumer.snapshot(0, 502), before);
        }
        consumer.apply_delta(&valid, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 502), producer.snapshot(0, 502));
    }

    #[test]
    fn capped_history_scroll_requires_complete_replacement() {
        let limits = GridLimits { scrollback_rows: 1 };
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"old\r\nretained\r\nlive");
        let before = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"\r\nnext");
        let delta = crate::GridDeltaBatch::between(&before, &producer.snapshot(0, 2)).unwrap();
        assert_eq!(delta.scrollback_rows, before.scrollback_rows);
        assert_ne!(delta.total_scrolled_rows, before.total_scrolled_rows);
        assert!(delta.reset_rows);
        assert!(matches!(
            consumer.apply_delta(&delta, limits),
            Err(super::TerminalGridStreamDeltaError::IncompleteHistory { .. })
        ));
        assert_eq!(consumer.snapshot(0, 3), before);
        let mut malformed = delta.clone();
        malformed.reset_rows = false;
        assert!(matches!(
            consumer.apply_delta(&malformed, limits),
            Err(super::TerminalGridStreamDeltaError::Delta(
                crate::GridDeltaApplyError::MissingReplacementRows
            ))
        ));
        assert_eq!(consumer.snapshot(0, 3), before);
        let after = producer.snapshot(0, 3);
        let complete = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(complete.reset_rows);
        consumer.apply_delta(&complete, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), after);
        let sparse = producer.process_delta(b"!").unwrap();
        assert!(!sparse.reset_rows);
        consumer.apply_delta(&sparse, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn cumulative_scroll_position_survives_eviction_alternate_and_delta_hydration() {
        let limits = GridLimits { scrollback_rows: 1 };
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        for _ in 0..10 {
            producer.process(b"line\r\n");
        }
        let total = producer.grid().total_scrolled_rows();
        assert!(total > 1);
        for alternate in [false, true] {
            if alternate {
                producer.process(b"\x1b[?1049h");
            }
            let snapshot = producer.snapshot(0, 3);
            let wire = serde_json::to_vec(&snapshot).unwrap();
            let decoded = serde_json::from_slice(&wire).unwrap();
            let mut consumer = TerminalGridStream::from_snapshot(&decoded, limits).unwrap();
            assert_eq!(consumer.grid().total_scrolled_rows(), total);
            let delta = producer.process_delta(b"x").unwrap();
            let wire = serde_json::to_vec(&delta).unwrap();
            let decoded = serde_json::from_slice(&wire).unwrap();
            consumer.apply_delta(&decoded, limits).unwrap();
            assert_eq!(consumer.grid().total_scrolled_rows(), total);
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn legacy_scroll_position_payloads_use_retained_count_fallback() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"one\r\ntwo\r\nthree");
        let mut wire = serde_json::to_value(producer.snapshot(0, 3)).unwrap();
        wire.as_object_mut().unwrap().remove("total_scrolled_rows");
        let snapshot: crate::GridSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(snapshot.total_scrolled_rows, None);
        let mut consumer = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
        assert_eq!(consumer.grid().total_scrolled_rows(), 1);
        let delta = producer.process_delta(b"!").unwrap();
        let mut wire = serde_json::to_value(delta).unwrap();
        wire.as_object_mut().unwrap().remove("total_scrolled_rows");
        let delta = serde_json::from_value(wire).unwrap();
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.grid().total_scrolled_rows(), 1);
    }

    #[test]
    fn alternate_history_retention_failure_preserves_state_and_allows_retry() {
        for supplied_backing in [false, true] {
            let limits = GridLimits::default();
            let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
            producer.process(b"history\r\nsecond\r\nthird\x1b[?1049halt");
            let before = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            let mut delta = producer.process_delta(b" update").unwrap();
            if supplied_backing {
                delta.main_rows = producer.snapshot(0, 3).main_rows;
            }
            assert!(matches!(
                consumer.apply_delta(&delta, GridLimits { scrollback_rows: 0 }),
                Err(super::TerminalGridStreamDeltaError::IncompleteHistory {
                    expected_rows: 1,
                    reconstructed_rows: 0,
                })
            ));
            assert_eq!(consumer.snapshot(0, 3), before);
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            producer.process(b"\x1b[?1049l continued");
            consumer.process(b"\x1b[?1049l continued");
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn alternate_sparse_deltas_preserve_omitted_main_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"history\r\nsecond\r\nthird\x1b[?1049halt");
        let before = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        for bytes in [b" update".as_slice(), b"\x1b[", b"31mred", b"\x1b7"] {
            let delta = producer.process_delta(bytes).unwrap();
            assert!(!delta.reset_rows);
            assert!(delta.main_rows.is_none());
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            assert_eq!(consumer.snapshot(0, 3).main_rows, before.main_rows);
        }
        producer.process(b"\x1b[?1049l\r\ncontinued");
        consumer.process(b"\x1b[?1049l\r\ncontinued");
        assert_eq!(consumer.snapshot(0, 5), producer.snapshot(0, 5));
    }

    #[test]
    fn sparse_viewport_deltas_preserve_hydrated_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"history\r\nsecond\r\nthird");
        let snapshot = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
        for bytes in [b"\x1b[Hchanged".as_slice(), b"\x1b[", b"31mred", b"\x1b7"] {
            let delta = producer.process_delta(bytes).unwrap();
            assert!(!delta.reset_rows);
            assert!(delta.row_updates.iter().all(|update| update.row_index < 2));
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            assert_eq!(consumer.snapshot(0, 3).rows[0], snapshot.rows[0]);
        }
        producer.process(b"\r\ncontinued");
        consumer.process(b"\r\ncontinued");
        assert_eq!(consumer.snapshot(0, 5), producer.snapshot(0, 5));
    }

    #[test]
    fn incomplete_scrolling_delta_preserves_consumer_state() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(&[b'x'; 40]);
        let before = producer.snapshot(0, 2);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"x");
        let delta = crate::GridDeltaBatch::between(&before, &producer.snapshot(0, 2)).unwrap();
        assert!(delta.scrollback_rows > 0);
        assert!(matches!(
            consumer.apply_delta(&delta, limits),
            Err(super::TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: 1,
                reconstructed_rows: 0,
            })
        ));
        assert_eq!(consumer.snapshot(0, 2), before);
        let complete = crate::GridDeltaBatch::between(&before, &producer.snapshot(0, 3)).unwrap();
        assert!(complete.reset_rows);
        consumer.apply_delta(&complete, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));

        // Rejection preserves parser/grid continuity, including pending wrap.
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        assert!(consumer.apply_delta(&delta, limits).is_err());
        consumer.process(b"x");
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn sparse_wrapped_history_converges_after_independent_reflow() {
        let limits = GridLimits::default();
        for text in ["abcdefghijklmnopqrstuvwxyz", "界界界界界界界界界界界界界"] {
            for alternate in [false, true] {
                let mut producer = TerminalGridStream::new(10, 2, limits).unwrap();
                producer.process(text.as_bytes());
                assert!(producer.grid().scrollback_rows_hint() > 0);
                if alternate {
                    producer.process(b"\x1b[?1049halt");
                }
                let snapshot = producer.snapshot(0, 20);
                let mut consumer = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
                for bytes in [b"\x1b[".as_slice(), b"31m", b"!", b"\x1b7"] {
                    let delta = producer.process_delta(bytes).unwrap();
                    assert!(!delta.reset_rows);
                    consumer.apply_delta(&delta, limits).unwrap();
                    assert_eq!(consumer.snapshot(0, 20), producer.snapshot(0, 20));
                }
                for width in [6, 13, 24] {
                    let mut expected = TerminalGridStream::from_grid(producer.grid().clone());
                    let mut actual = TerminalGridStream::from_grid(consumer.grid().clone());
                    expected.resize(width, 4).unwrap();
                    actual.resize(width, 4).unwrap();
                    expected.process(b"\x1b[?1049l continued");
                    actual.process(b"\x1b[?1049l continued");
                    assert_eq!(actual.snapshot(0, 20), expected.snapshot(0, 20));
                }
            }
        }
    }

    #[test]
    fn complete_scrolling_replacement_preserves_backing_history() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(&[b'x'; 40]);
        let before = producer.snapshot(0, 2);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        producer.process(b"y");
        let after = producer.snapshot(0, 3);
        let delta = crate::GridDeltaBatch::between(&before, &after).unwrap();
        assert!(delta.reset_rows);
        assert_eq!(delta.scrollback_rows, 1);
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), after);
        // Hydration must preserve the backing content, not just its count.
        for bytes in [b"z".as_slice(), b"\r\nnext"] {
            producer.process(bytes);
            consumer.process(bytes);
            assert_eq!(consumer.snapshot(0, 5), producer.snapshot(0, 5));
        }
    }

    #[test]
    fn replacement_exceeding_history_retention_preserves_consumer() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 2, limits).unwrap();
        producer.process(b"first\r\nsecond\x1b[");
        let before = producer.snapshot(0, 2);
        producer.process(b"31m\r\nthird");
        let after = producer.snapshot(0, 3);
        let delta = crate::GridDeltaBatch::between(&before, &after).unwrap();
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        assert!(matches!(
            consumer.apply_delta(&delta, GridLimits { scrollback_rows: 0 }),
            Err(super::TerminalGridStreamDeltaError::IncompleteHistory {
                expected_rows: 1,
                reconstructed_rows: 0,
            })
        ));
        assert_eq!(consumer.snapshot(0, 2), before);
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), after);

        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        assert!(
            consumer
                .apply_delta(&delta, GridLimits { scrollback_rows: 0 })
                .is_err()
        );
        consumer.process(b"31m\r\nthird");
        assert_eq!(consumer.snapshot(0, 3), after);
        producer.process(b"\r\nfourth\r\nfifth");
        consumer.process(b"\r\nfourth\r\nfifth");
        assert_eq!(consumer.snapshot(0, 5), producer.snapshot(0, 5));
        assert_eq!(consumer.snapshot(0, 5).scrollback_rows, 3);
    }

    #[test]
    fn ignored_input_does_not_publish_delta_but_prefix_changes_do() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
        producer.process(b"main");
        let before = producer.snapshot(0, 3);
        assert!(producer.process_delta(b"\x00").is_none());
        assert_eq!(producer.snapshot(0, 3), before);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        for bytes in [b"\x1b[".as_slice(), b"31mred"] {
            let delta = producer.process_delta(bytes).unwrap();
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn empty_input_preserves_pending_sequence_and_revision() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
        producer.process(b"main\x1b[?1049halt\x1b[");
        let before = producer.snapshot(0, 3);
        let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
        assert!(producer.process_delta(b"").is_none());
        assert_eq!(producer.snapshot(0, 3), before);
        let delta = producer.process_delta(b"31mred").unwrap();
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn failed_resize_preserves_replication_and_parser_continuity() {
        let limits = GridLimits::default();
        for (width, height) in [(0, 3), (20, 0), (0, 0)] {
            let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
            producer.process(b"main\x1b[?1049halt\x1b[");
            let before = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            assert!(matches!(
                producer.resize_delta(width, height),
                Err(crate::TerminalGridError::ZeroDimensions)
            ));
            assert_eq!(producer.snapshot(0, 3), before);
            let delta = producer.process_delta(b"31mred").unwrap();
            assert_eq!(delta.base_revision, before.revision);
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            producer.process(b"\x1b[?1049l!");
            consumer.process(b"\x1b[?1049l!");
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn standalone_cursor_save_replicates_without_content_damage() {
        for (save, restore) in [
            (b"\x1b7".as_slice(), b"\x1b8".as_slice()),
            (b"\x1b[s", b"\x1b[u"),
        ] {
            let limits = GridLimits::default();
            let mut producer = TerminalGridStream::new(10, 3, limits).unwrap();
            producer.process(b"1234567890");
            let before = producer.snapshot(0, 3);
            let mut consumer = TerminalGridStream::from_snapshot(&before, limits).unwrap();
            let delta = producer
                .process_delta(save)
                .expect("saving cursor changes replicated state");
            assert_eq!(delta.content_revision, before.content_revision);
            assert!(delta.row_updates.is_empty());
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
            for stream in [&mut producer, &mut consumer] {
                stream.process(b"\x1b[3;1Hother");
                stream.process(restore);
                stream.process(b"X");
            }
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
    }

    #[test]
    fn prefix_only_deltas_preserve_split_sequence_continuity() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 3, limits).unwrap();
        let mut consumer = TerminalGridStream::new(20, 3, limits).unwrap();
        let initial_content_revision = producer.grid().content_revision();
        for prefix in [b"\x1b".as_slice(), b"[", b"31"] {
            let delta = producer
                .process_delta(prefix)
                .expect("prefix changes replication state");
            assert_eq!(producer.grid().content_revision(), initial_content_revision);
            consumer.apply_delta(&delta, limits).unwrap();
            assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        }
        let delta = producer.process_delta(b"mred").unwrap();
        consumer.apply_delta(&delta, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
        assert!(producer.process_delta(b"").is_none());
        // Hydration must preserve enough parser state to continue raw input too.
        let delta = producer.process_delta(b"\x1b[").unwrap();
        consumer.apply_delta(&delta, limits).unwrap();
        producer.process(b"0mplain");
        consumer.process(b"0mplain");
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }

    #[test]
    fn stream_preserves_split_escape_sequence() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        stream.process(b"\x1b[");
        stream.process(b"31mR");

        let grid = stream.grid();
        let red = grid.viewport_rows()[0].cells()[0].style();
        assert_ne!(red, crate::style::StyleId::DEFAULT);
        assert_eq!(
            grid.palette().get(red).fg,
            Some(crate::style::Color::Indexed(1))
        );
    }

    #[test]
    fn decset_1049_restores_main_screen_cursor_on_exit() {
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        stream.process(b"\x1b[12;34H\x1b[?1049hALT\x1b[?1049l");

        let cursor = stream.grid().cursor();
        assert_eq!((cursor.row, cursor.col), (11, 33));
        assert_eq!(stream.grid().mode(), crate::model::GridMode::Main);
    }

    #[test]
    fn cursor_save_restore_variants_restore_saved_position() {
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();

        stream.process(b"\x1b[3;4H\x1b7\x1b[9;10H\x1b8");
        let cursor = stream.grid().cursor();
        assert_eq!((cursor.row, cursor.col), (2, 3));

        stream.process(b"\x1b[5;6H\x1b[s\x1b[11;12H\x1b[u");
        let cursor = stream.grid().cursor();
        assert_eq!((cursor.row, cursor.col), (4, 5));
    }

    #[test]
    fn cursor_save_restore_variants_restore_pending_wrap_state() {
        let mut stream = TerminalGridStream::new(5, 3, GridLimits::default()).unwrap();

        stream.process(b"AB\x1b7\x1b[1;5H!");
        assert!(stream.grid().pending_wrap());

        stream.process(b"\x1b8C");
        let rows = stream.grid().viewport_rows();
        assert_eq!(row_text(&rows[0]), "ABC !");
        assert_eq!(stream.grid().cursor().row, 0);
        assert_eq!(stream.grid().cursor().col, 3);
    }

    #[test]
    fn decset_1049_resize_exit_keeps_shell_output_live() {
        let mut stream = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        stream.process(b"shell\r\nready");
        stream.process(b"\x1b[?1049hALT");

        stream.resize(16, 6).unwrap();
        stream.process(b"\x1b[?1049l\r\nPROMPT> echo alive\r\nalive");

        assert_eq!(stream.grid().mode(), crate::model::GridMode::Main);
        assert_eq!(stream.grid().viewport_rows().len(), 6);
        let text = crate::visible_text(stream.grid(), 0, 6);
        assert!(text.contains("PROMPT>"));
        assert!(text.contains("alive"));
    }

    #[test]
    fn alternate_screen_modes_remain_writable_after_resize_and_exit() {
        for mode in [47, 1047, 1049] {
            let mut stream = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
            stream.process(b"main");
            stream.process(format!("\x1b[?{mode}hALT").as_bytes());
            stream.resize(14, 5).unwrap();
            stream.process(format!("\x1b[?{mode}l\r\nLIVE").as_bytes());

            assert_eq!(stream.grid().mode(), crate::model::GridMode::Main);
            assert_eq!(stream.grid().viewport_rows().len(), 5);
            assert!(crate::visible_text(stream.grid(), 0, 5).contains("LIVE"));
        }
    }

    #[test]
    fn cursor_visibility_is_structured_state_and_survives_snapshot() {
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        stream.process(b"\x1b[?25l");
        assert!(!stream.grid().cursor().visible);

        let snapshot = stream.snapshot(0, 24);
        assert!(!snapshot.cursor.visible);
        let hydrated = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default())
            .expect("snapshot should hydrate");
        assert!(!hydrated.grid().cursor().visible);

        stream.process(b"\x1b[?25h");
        assert!(stream.grid().cursor().visible);
    }

    #[test]
    fn scroll_region_reset_restores_full_viewport_scrolling() {
        let mut grid = TerminalGrid::new(5, 4, GridLimits::default()).unwrap();
        grid.process(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        grid.process(b"\x1b[2;3r\x1b[3;1H\n");
        let region_rows = grid.viewport_rows();
        assert_eq!(row_text(&region_rows[0]), "aaaa");
        assert_eq!(row_text(&region_rows[1]), "cccc");
        assert_eq!(row_text(&region_rows[2]), "");
        assert_eq!(row_text(&region_rows[3]), "dddd");

        grid.process(b"\x1b[r\x1b[4;1H\n");
        let reset_rows = grid.viewport_rows();
        assert_eq!(row_text(&reset_rows[0]), "cccc");
        assert_eq!(row_text(&reset_rows[1]), "");
        assert_eq!(row_text(&reset_rows[2]), "dddd");
        assert_eq!(row_text(&reset_rows[3]), "");
    }

    #[test]
    fn snapshot_hydrates_pending_escape_sequence() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        stream.process(b"\x1b[");
        let snapshot = stream.snapshot(0, 2);

        let mut hydrated = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default())
            .expect("snapshot should hydrate");
        hydrated.process(b"31mR");

        let grid = hydrated.grid();
        let red = grid.viewport_rows()[0].cells()[0].style();
        assert_eq!(
            grid.palette().get(red).fg,
            Some(crate::style::Color::Indexed(1))
        );
    }

    #[test]
    fn delta_carries_pending_escape_after_visible_output() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();

        let delta = stream
            .process_delta(b"A\x1b[")
            .expect("visible output should produce a delta");

        assert_eq!(delta.pending_bytes, b"\x1b[");
    }

    #[test]
    fn protocol_state_tracks_mouse_and_input_modes() {
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        stream.process(b"\x1b[?1000h\x1b[?1006h\x1b[?1;2004h\x1b=");

        let protocol = stream.grid().protocol_state();
        assert_eq!(
            protocol.mouse_mode(),
            crate::model::MouseProtocolMode::PressRelease
        );
        assert_eq!(
            protocol.mouse_encoding(),
            crate::model::MouseProtocolEncoding::Sgr
        );
        assert!(protocol.application_cursor);
        assert!(protocol.application_keypad);
        assert!(protocol.bracketed_paste);

        stream.process(b"\x1b[?1000l\x1b[?1006l\x1b[?1;2004l\x1b>");
        let protocol = stream.grid().protocol_state();
        assert_eq!(protocol.mouse_mode(), crate::model::MouseProtocolMode::None);
        assert_eq!(
            protocol.mouse_encoding(),
            crate::model::MouseProtocolEncoding::Default
        );
        assert!(!protocol.application_cursor);
        assert!(!protocol.application_keypad);
        assert!(!protocol.bracketed_paste);
    }

    #[test]
    fn alternate_snapshot_and_delta_preserve_main_screen_for_independent_sizes() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 6, limits).unwrap();
        producer.process(b"first line\r\nsecond line\r\nthird line");
        producer.process(b"\x1b[?1049happ");
        let snapshot = producer.snapshot(0, 6);
        assert!(snapshot.main_rows.is_some());
        let mut replica = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
        let delta = producer.process_delta(b" updated").unwrap();
        // Normal alternate-screen frames must not retransmit the hidden screen.
        assert!(delta.main_rows.is_none());
        replica.apply_delta(&delta, limits).unwrap();

        for (width, height) in [(20, 6), (10, 8), (30, 4)] {
            let mut expected = TerminalGridStream::new(20, 6, limits).unwrap();
            expected.process(b"first line\r\nsecond line\r\nthird line");
            expected.process(b"\x1b[?1049happ updated");
            let mut actual =
                TerminalGridStream::from_snapshot(&replica.snapshot(0, 6), limits).unwrap();
            expected.resize(width, height).unwrap();
            actual.resize(width, height).unwrap();
            expected.process(b"\x1b[?1049l\r\nprompt> ");
            actual.process(b"\x1b[?1049l\r\nprompt> ");
            assert_eq!(
                actual.grid().viewport_rows(),
                expected.grid().viewport_rows()
            );
            assert_eq!(actual.grid().cursor(), expected.grid().cursor());
        }
        assert_eq!((replica.grid().width(), replica.grid().height()), (20, 6));
    }

    #[test]
    fn alternate_entry_delta_preserves_main_screen_on_raw_exit() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 6, limits).unwrap();
        producer.process(b"retained main content");
        let mut replica =
            TerminalGridStream::from_snapshot(&producer.snapshot(0, 6), limits).unwrap();
        let delta = producer.process_delta(b"\x1b[?1049happ").unwrap();
        assert!(delta.main_rows.is_some());
        replica.apply_delta(&delta, limits).unwrap();
        producer.process(b"\x1b[?1049l");
        replica.process(b"\x1b[?1049l");
        assert_eq!(replica.snapshot(0, 6), producer.snapshot(0, 6));
    }

    #[test]
    fn snapshot_and_delta_converge_after_content_resize() {
        let limits = GridLimits::default();
        let mut producer = TerminalGridStream::new(20, 6, limits).unwrap();
        producer.process(b"before\r\nresize");
        producer.resize(10, 4).expect("resize producer");
        let baseline = producer.snapshot(0, 4);
        let mut consumer =
            TerminalGridStream::from_snapshot(&baseline, limits).expect("hydrate resized baseline");
        let delta = producer
            .process_delta(b"\x1b[4;1Hafter")
            .expect("post-resize output delta");

        consumer
            .apply_delta(&delta, limits)
            .expect("apply post-resize delta");

        assert_eq!(consumer.snapshot(0, 4), producer.snapshot(0, 4));
        assert_eq!((consumer.grid().width(), consumer.grid().height()), (10, 4));
    }

    #[test]
    fn snapshot_and_delta_preserve_protocol_state() {
        let mut producer = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        let delta = producer
            .process_delta(b"\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b=")
            .expect("protocol-only change should produce a delta");
        let mut consumer = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        consumer
            .apply_delta(&delta, GridLimits::default())
            .expect("protocol delta should apply");

        let protocol = consumer.grid().protocol_state();
        assert_eq!(
            protocol.mouse_mode(),
            crate::model::MouseProtocolMode::AnyMotion
        );
        assert_eq!(
            protocol.mouse_encoding(),
            crate::model::MouseProtocolEncoding::Sgr
        );
        assert!(protocol.application_keypad);
        assert!(protocol.bracketed_paste);

        let snapshot = producer.snapshot(0, 24);
        let restored = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default())
            .expect("protocol snapshot should hydrate");
        assert!(restored.grid().protocol_state().bracketed_paste);

        producer.process(b"\x1bc");
        assert!(!producer.grid().protocol_state().bracketed_paste);
    }

    #[test]
    fn apply_delta_preserves_pending_escape_for_future_raw_chunks() {
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let delta = producer
            .process_delta(b"A\x1b[")
            .expect("visible output should produce a delta");
        let mut consumer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();

        consumer
            .apply_delta(&delta, GridLimits::default())
            .expect("delta should apply");
        consumer.process(b"31mR");

        let grid = consumer.grid();
        let red = grid.viewport_rows()[0].cells()[1].style();
        assert_eq!(row_text(&grid.viewport_rows()[0]), "AR");
        assert_eq!(
            grid.palette().get(red).fg,
            Some(crate::style::Color::Indexed(1))
        );
    }

    #[test]
    fn insert_and_delete_character_sequences_shift_row_cells() {
        let mut grid = TerminalGrid::new(5, 2, GridLimits::default()).unwrap();
        grid.process(b"abcd\x1b[1;2H\x1b[@Z");
        assert_eq!(row_text(&grid.viewport_rows()[0]), "aZbcd");

        grid.process(b"\x1b[1;2H\x1b[P");
        assert_eq!(row_text(&grid.viewport_rows()[0]), "abcd");
    }

    #[test]
    fn insert_and_delete_line_sequences_shift_scroll_region() {
        let mut grid = TerminalGrid::new(5, 4, GridLimits::default()).unwrap();
        grid.process(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        grid.process(b"\x1b[2;3r\x1b[2;1H\x1b[L");
        let rows = grid.viewport_rows();
        assert_eq!(row_text(&rows[0]), "aaaa");
        assert_eq!(row_text(&rows[1]), "");
        assert_eq!(row_text(&rows[2]), "bbbb");
        assert_eq!(row_text(&rows[3]), "dddd");

        grid.process(b"\x1b[2;1H\x1b[M");
        let rows = grid.viewport_rows();
        assert_eq!(row_text(&rows[1]), "bbbb");
        assert_eq!(row_text(&rows[2]), "");
    }

    #[test]
    fn linefeed_scrolls_only_active_scroll_region() {
        let mut grid = TerminalGrid::new(5, 4, GridLimits::default()).unwrap();
        grid.process(b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        grid.process(b"\x1b[2;3r\x1b[3;1H\n");

        let rows = grid.viewport_rows();
        assert_eq!(row_text(&rows[0]), "aaaa");
        assert_eq!(row_text(&rows[1]), "cccc");
        assert_eq!(row_text(&rows[2]), "");
        assert_eq!(row_text(&rows[3]), "dddd");
    }

    fn row_text(row: &crate::model::PhysicalRow) -> String {
        row.cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(crate::model::Cell::text)
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}
