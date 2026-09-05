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
        let mut continuity = self.pending_bytes.clone();
        continuity.extend_from_slice(bytes);
        self.pending_bytes = trailing_incomplete_sequence(&continuity);
        let mut performer = GridPerformer {
            grid: &mut self.grid,
        };
        self.parser.advance(&mut performer, bytes);
    }

    /// Snapshot the grid plus parser-prefix bytes needed to continue a split
    /// terminal sequence from a newly hydrated stream.
    #[must_use]
    pub fn snapshot(&self, scrollback_offset: usize, rows: usize) -> crate::GridSnapshot {
        let mut snapshot = self.grid.snapshot(scrollback_offset, rows);
        snapshot.pending_bytes.clone_from(&self.pending_bytes);
        snapshot
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
        let mut snapshot = self.snapshot(0, self.grid.height());
        delta.apply_to_snapshot(&mut snapshot)?;
        *self = Self::from_snapshot(&snapshot, limits)?;
        Ok(())
    }

    /// Process one chunk and return a structured row delta when state changed.
    #[must_use]
    pub fn process_delta(&mut self, bytes: &[u8]) -> Option<GridDeltaBatch> {
        let before = self.snapshot(0, self.grid.height());
        self.process(bytes);
        let after = self.snapshot(0, self.grid.height());
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
        if self.grid.width() == usize::from(width) && self.grid.height() == usize::from(height) {
            return Ok(None);
        }
        let before = self.snapshot(0, self.grid.height());
        self.grid.resize(width, height)?;
        let after = self.snapshot(0, self.grid.height());
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

    pub fn set_protocol_state(&mut self, protocol: ProtocolState) {
        self.protocol = protocol;
    }

    pub fn set_alternate_screen(&mut self, alternate_screen: bool) {
        self.alternate_screen = alternate_screen;
    }

    pub fn process(&mut self, bytes: &[u8]) -> ProtocolProcessOutcome {
        let mut continuity = self.pending_bytes.clone();
        continuity.extend_from_slice(bytes);
        self.pending_bytes = trailing_incomplete_sequence(&continuity);
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
            b'c' => {
                let width = u16::try_from(self.grid.width()).unwrap_or(u16::MAX);
                let height = u16::try_from(self.grid.height()).unwrap_or(u16::MAX);
                if let Ok(reset) =
                    TerminalGrid::new(width, height, crate::model::GridLimits::default())
                {
                    *self.grid = reset;
                }
            }
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
    let utf8_pending_start = match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
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
