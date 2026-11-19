use crate::model::{
    GridLimits, GridMode, MouseProtocolEncoding, MouseProtocolMode, ProtocolState, TerminalGrid,
    TerminalGridError,
};
use crate::vt_parser::{Params, Parser, Perform};
use bmux_terminal_protocol::pointer_shape::{
    PointerShape, PointerShapeOperation, parse_pointer_shape,
};

/// Streaming terminal parser plus structured grid state.
///
/// Unlike [`TerminalGrid::process`](crate::TerminalGrid::process), this type
/// owns the streaming parser state and therefore preserves incomplete escape
/// sequences across PTY chunk boundaries.
pub struct TerminalGridStream {
    parser: Parser,
    grid: TerminalGrid,
    pending_bytes: Vec<u8>,
    /// The prefix before the last chunk, kept to compare with and to reuse its storage.
    previous_pending: Vec<u8>,
    /// A DCS string an observer asked for, collected across chunks.
    observed_dcs: ObservedDcs,
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
            parser: Parser::new(),
            grid,
            pending_bytes: Vec::new(),
            previous_pending: Vec::new(),
            observed_dcs: ObservedDcs::default(),
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
            // The snapshot already contains effects of controls executed inside
            // this prefix. Replay only to reconstruct decoder state.
            stream
                .parser
                .advance(&mut ParserRestore, &snapshot.pending_bytes);
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
        self.process_observed(bytes, &mut IgnoreSequences);
    }

    /// Process output and synchronously observe each complete OSC 22 query.
    /// The callback receives the query list (without `?`) and the canonical
    /// active-screen request at that exact byte position. It must not reenter
    /// this stream. Replies and default/grabbed policy remain caller-owned.
    /// Snapshot restoration never invokes this callback.
    pub fn process_with_pointer_queries(
        &mut self,
        bytes: &[u8],
        on_query: impl FnMut(&[u8], Option<PointerShape>),
    ) {
        self.process_observed(bytes, &mut PointerQueries(on_query));
    }

    /// Process output, showing `observer` each sequence in order as the grid applies it (see
    /// [`SequenceObserver`]). Snapshot restoration never shows an observer anything.
    pub fn process_observed<O: SequenceObserver>(&mut self, bytes: &[u8], observer: &mut O) {
        if bytes.is_empty() {
            return;
        }
        let revision = self.grid.revision();
        std::mem::swap(&mut self.pending_bytes, &mut self.previous_pending);
        let mut performer = GridPerformer {
            grid: &mut self.grid,
            observer,
            dcs: &mut self.observed_dcs,
        };
        self.parser.advance(&mut performer, bytes);
        // Written into the storage of the prefix before last: normal text writes nothing.
        self.parser.write_replay_prefix(&mut self.pending_bytes);
        // Parser prefixes are replicated state even when no terminal operation
        // completed. Do not mark visible content dirty for a prefix-only update.
        if self.pending_bytes != self.previous_pending && self.grid.revision() == revision {
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

    /// Resize the grid without computing a structured delta.
    ///
    /// # Errors
    ///
    /// Returns an error if width or height is zero.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), TerminalGridError> {
        self.grid.resize(width, height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolProcessOutcome {
    pub toggled_alternate: bool,
    /// A synchronized update (DEC mode 2026) ended in these bytes: the application finished a
    /// frame, which can be shown now.
    pub ended_synchronized_update: bool,
}

/// Streaming parser for terminal protocol/input hints without retaining pane cells.
pub struct TerminalProtocolTracker {
    parser: Parser,
    protocol: ProtocolState,
    alternate_screen: bool,
    /// Inside a synchronized update (DEC mode 2026): the application has begun a frame and not
    /// finished it. Transient presentation state, not part of [`ProtocolState`].
    synchronized_update: bool,
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
            parser: Parser::new(),
            protocol: ProtocolState::default(),
            alternate_screen: false,
            synchronized_update: false,
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

    /// Whether the bytes processed so far leave a synchronized update (DEC mode 2026) open.
    #[must_use]
    pub const fn synchronized_update(&self) -> bool {
        self.synchronized_update
    }

    /// Set whether a synchronized update is open, from an authoritative watermark (a snapshot
    /// taken at a known stream offset).
    pub const fn set_synchronized_update(&mut self, open: bool) {
        self.synchronized_update = open;
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
                ended_synchronized_update: false,
            };
        }
        let mut performer = ProtocolPerformer {
            protocol: &mut self.protocol,
            alternate_screen: &mut self.alternate_screen,
            synchronized_update: &mut self.synchronized_update,
            toggled_alternate: false,
            ended_synchronized_update: false,
        };
        self.parser.advance(&mut performer, bytes);
        self.parser.write_replay_prefix(&mut self.pending_bytes);
        ProtocolProcessOutcome {
            toggled_alternate: performer.toggled_alternate,
            ended_synchronized_update: performer.ended_synchronized_update,
        }
    }
}

pub(crate) fn process(grid: &mut TerminalGrid, bytes: &[u8]) {
    let mut parser = Parser::new();
    let mut performer = GridPerformer {
        grid,
        observer: &mut IgnoreSequences,
        dcs: &mut ObservedDcs::default(),
    };
    parser.advance(&mut performer, bytes);
}

/// The longest DCS string an observer is shown; a longer one is dropped.
pub const MAX_OBSERVED_DCS_BYTES: usize = 512;

/// What a caller observes of the sequences a [`TerminalGridStream`] parses, in order with the
/// grid's own handling: each call comes after the grid has applied the sequence, with the grid as
/// it then is (its cursor, its modes). Observers are statically dispatched, and every method
/// defaults to nothing, so a stream that observes nothing ([`TerminalGridStream::process`])
/// costs what it did without them.
pub trait SequenceObserver {
    /// A complete CSI sequence the parser did not ignore.
    fn csi(&mut self, _grid: &TerminalGrid, _sequence: &SequenceHeader<'_>) {}

    /// A complete OSC string, split at `;` into at most 16 parameters.
    fn osc(&mut self, _grid: &TerminalGrid, _params: &[&[u8]], _bell_terminated: bool) {}

    /// Whether to collect the DCS string `introducer` starts, for [`Self::dcs`]. Asked once per
    /// string; data is collected only when this says so.
    fn wants_dcs(&mut self, _introducer: &SequenceHeader<'_>) -> bool {
        false
    }

    /// A complete DCS string [`Self::wants_dcs`] asked for: its introducer's intermediates and
    /// final byte, and its data (at most [`MAX_OBSERVED_DCS_BYTES`]).
    fn dcs(&mut self, _grid: &TerminalGrid, _intermediates: &[u8], _action: char, _data: &[u8]) {}

    /// An OSC 22 pointer-shape query (its query list, without `?`) and the active screen's current
    /// shape. Replies and default/grabbed policy are the observer's.
    fn pointer_query(&mut self, _query: &[u8], _current: Option<PointerShape>) {}
}

/// An observer of nothing.
pub struct IgnoreSequences;

impl SequenceObserver for IgnoreSequences {}

/// An observer of pointer-shape queries only.
struct PointerQueries<F>(F);

impl<F: FnMut(&[u8], Option<PointerShape>)> SequenceObserver for PointerQueries<F> {
    fn pointer_query(&mut self, query: &[u8], current: Option<PointerShape>) {
        (self.0)(query, current);
    }
}

/// The header of a CSI sequence or DCS string: parameters, intermediates (a private marker such
/// as `?` or `>` first) and final byte.
pub struct SequenceHeader<'a> {
    params: &'a Params,
    intermediates: &'a [u8],
    action: char,
}

impl SequenceHeader<'_> {
    /// Private marker and intermediate bytes, in order.
    #[must_use]
    pub const fn intermediates(&self) -> &[u8] {
        self.intermediates
    }

    /// The final byte.
    #[must_use]
    pub const fn action(&self) -> char {
        self.action
    }

    /// The parameter values (one omitted is 0), or `None` when any has subparameters.
    #[must_use]
    pub fn values(&self) -> Option<HeaderValues> {
        let mut values = HeaderValues {
            values: [0; 32],
            len: 0,
        };
        for group in self.params {
            let [value] = group else {
                return None;
            };
            *values.values.get_mut(values.len)? = *value;
            values.len += 1;
        }
        Some(values)
    }
}

/// A sequence's parameter values, on the stack (a sequence holds at most 32).
pub struct HeaderValues {
    values: [u16; 32],
    len: usize,
}

impl std::ops::Deref for HeaderValues {
    type Target = [u16];

    fn deref(&self) -> &[u16] {
        &self.values[..self.len]
    }
}

/// A DCS string being collected for an observer.
#[derive(Default)]
struct ObservedDcs {
    collecting: bool,
    overflow: bool,
    intermediates: [u8; 2],
    intermediate_len: usize,
    action: char,
    data: Vec<u8>,
}

struct ParserRestore;

impl Perform for ParserRestore {}

struct ProtocolPerformer<'a> {
    protocol: &'a mut ProtocolState,
    alternate_screen: &'a mut bool,
    synchronized_update: &'a mut bool,
    toggled_alternate: bool,
    ended_synchronized_update: bool,
}

impl ProtocolPerformer<'_> {
    fn set_synchronized_update(&mut self, open: bool) {
        if *self.synchronized_update && !open {
            self.ended_synchronized_update = true;
        }
        *self.synchronized_update = open;
    }

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
    // Protocol state lives in escape sequences only, among CSIs in private ones.
    const OBSERVES_TEXT: bool = false;
    const OBSERVES_PLAIN_CSI: bool = false;

    fn print(&mut self, _c: char) {}

    fn execute(&mut self, _byte: u8) {}

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let [selector, payload] = params
            && *selector == b"22"
            && let Ok(operation) = parse_pointer_shape(payload)
        {
            self.protocol
                .pointer_shapes
                .apply(*self.alternate_screen, operation);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if !ignore && is_soft_reset(params, intermediates, action) {
            *self.protocol = ProtocolState {
                pointer_shapes: self.protocol.pointer_shapes,
                ..ProtocolState::default()
            };
            return;
        }
        if ignore || intermediates != *b"?" || !matches!(action, 'h' | 'l') {
            return;
        }
        let enabled = action == 'h';
        for value in flatten_params(params).iter().copied() {
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
                2026 => self.set_synchronized_update(enabled),
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
                self.set_synchronized_update(false);
            }
            _ => {}
        }
    }
}

struct GridPerformer<'a, O: SequenceObserver> {
    grid: &'a mut TerminalGrid,
    observer: &'a mut O,
    dcs: &'a mut ObservedDcs,
}

impl<O: SequenceObserver> Perform for GridPerformer<'_, O> {
    fn print(&mut self, c: char) {
        let ch = self.grid.characters.translate(c);
        self.grid.characters.last = Some(ch);
        self.grid.print_char(ch);
    }

    fn print_ascii(&mut self, text: &[u8]) {
        if self.grid.characters.translates() {
            for &byte in text {
                self.print(char::from(byte));
            }
            return;
        }
        if let Some(&last) = text.last() {
            self.grid.characters.last = Some(char::from(last));
        }
        self.grid.print_ascii(text);
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

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        let header = SequenceHeader {
            params,
            intermediates,
            action,
        };
        let dcs = &mut *self.dcs;
        dcs.collecting = !ignore && self.observer.wants_dcs(&header);
        if dcs.collecting {
            dcs.overflow = false;
            dcs.intermediate_len = intermediates.len().min(dcs.intermediates.len());
            dcs.intermediates[..dcs.intermediate_len]
                .copy_from_slice(&intermediates[..dcs.intermediate_len]);
            dcs.action = action;
            dcs.data.clear();
        }
    }

    fn put(&mut self, byte: u8) {
        let dcs = &mut *self.dcs;
        if dcs.collecting && !dcs.overflow {
            if dcs.data.len() < MAX_OBSERVED_DCS_BYTES {
                dcs.data.push(byte);
            } else {
                dcs.overflow = true;
            }
        }
    }

    fn unhook(&mut self) {
        let dcs = &mut *self.dcs;
        if std::mem::take(&mut dcs.collecting) && !dcs.overflow {
            self.observer.dcs(
                self.grid,
                &dcs.intermediates[..dcs.intermediate_len],
                dcs.action,
                &dcs.data,
            );
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.apply_osc(params);
        self.observer.osc(self.grid, params, bell_terminated);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.apply_csi(params, intermediates, ignore, action);
        if !ignore {
            self.observer.csi(
                self.grid,
                &SequenceHeader {
                    params,
                    intermediates,
                    action,
                },
            );
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.apply_esc(intermediates, ignore, byte);
    }
}

impl<O: SequenceObserver> GridPerformer<'_, O> {
    fn apply_osc(&mut self, params: &[&[u8]]) {
        if let [selector, payload] = params
            && *selector == b"22"
            && let Ok(operation) = parse_pointer_shape(payload)
        {
            if let PointerShapeOperation::Query(query) = operation {
                let current = self
                    .grid
                    .protocol_state()
                    .pointer_shapes
                    .current(self.grid.mode() == GridMode::Alternate);
                self.observer.pointer_query(query, current);
            } else {
                self.grid.apply_pointer_shape(operation);
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "single CSI dispatcher keeps terminal escape handling in one explicit state-machine branch"
    )]
    fn apply_csi(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if !ignore && is_soft_reset(params, intermediates, action) {
            self.grid.soft_reset_input_modes();
            return;
        }
        if ignore
            || !(intermediates.is_empty() || matches!(action, 'h' | 'l') && intermediates == b"?")
            || (action != 'm' && params.iter().any(|group| group.len() != 1))
        {
            return;
        }
        if action == 'm' {
            self.grid.set_graphic_rendition(&sgr_params(params));
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
            'd' => self
                .grid
                .move_cursor_row(one_based(values.first()).saturating_sub(1)),
            'b' => {
                if let Some(ch) = self.grid.characters.last {
                    for _ in 0..one_based(values.first()) {
                        self.grid.print_char(ch);
                    }
                }
            }
            'G' | '`' => self
                .grid
                .move_cursor_column(one_based(values.first()).saturating_sub(1)),
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
                let (top, bottom) = self
                    .grid
                    .scroll_region()
                    .unwrap_or_else(|| (0, self.grid.height().saturating_sub(1)));
                self.grid
                    .scroll_region_up(top, bottom, one_based(values.first()));
            }
            'T' => {
                let (top, bottom) = self
                    .grid
                    .scroll_region()
                    .unwrap_or_else(|| (0, self.grid.height().saturating_sub(1)));
                self.grid
                    .scroll_region_down(top, bottom, one_based(values.first()));
            }
            'X' => self.grid.erase_chars(one_based(values.first())),
            '@' => self.grid.insert_blank_chars(one_based(values.first())),
            'g' => self.grid.clear_tab_stops(default_zero(values.first())),
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),
            'r' => {
                if values.is_empty() {
                    self.grid.set_scroll_region(None, None);
                } else {
                    let top = one_based(values.first()).saturating_sub(1);
                    let bottom = values
                        .get(1)
                        .filter(|&&value| value != 0)
                        .map(|value| one_based(Some(value)).saturating_sub(1));
                    self.grid.set_scroll_region(Some(top), bottom);
                }
            }
            'h' | 'l' if intermediates == *b"?" => {
                let enabled = action == 'h';
                for value in values.iter().copied() {
                    match value {
                        1 => self.grid.set_application_cursor(enabled),
                        6 => self.grid.set_origin_mode(enabled),
                        7 => self.grid.set_autowrap(enabled),
                        9 => self
                            .grid
                            .set_mouse_tracking_mode(MouseProtocolMode::Press, enabled),
                        25 => self.grid.set_cursor_visible(enabled),
                        47 | 1047 | 1049 => self.grid.set_alternate_mode(value, enabled),
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
            'h' | 'l' if intermediates.is_empty() => {
                for value in values.iter().copied() {
                    if value == 4 {
                        self.grid.set_insert_mode(action == 'h');
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_esc(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
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
            b'H' => self.grid.set_tab_stop(),
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

/// Whether a CSI sequence is a soft terminal reset (DECSTR, `CSI ! p`), which takes no parameters
/// (the parser reports their absence as one default zero).
fn is_soft_reset(params: &Params, intermediates: &[u8], action: char) -> bool {
    action == 'p'
        && intermediates == b"!"
        && params
            .iter()
            .all(|group| group.iter().all(|value| *value == 0))
        && params.iter().count() <= 1
}

/// Parameter values of one escape sequence, on the stack: a sequence holds at most 32 values
/// (the parser's bound), so nothing it normalizes or flattens to needs more than this.
pub(crate) struct ParamValues {
    values: [i64; 64],
    len: usize,
}

impl ParamValues {
    const fn new() -> Self {
        Self {
            values: [0; 64],
            len: 0,
        }
    }

    const fn push(&mut self, value: i64) {
        if self.len < self.values.len() {
            self.values[self.len] = value;
            self.len += 1;
        }
    }

    pub(crate) fn as_slice(&self) -> &[i64] {
        &self.values[..self.len]
    }
}

impl std::ops::Deref for ParamValues {
    type Target = [i64];

    fn deref(&self) -> &[i64] {
        self.as_slice()
    }
}

/// Normalize SGR groups without interpreting color operands as independent
/// commands. Colons belong to a single operation; semicolons separate operations
/// except for the legacy extended-color forms.
fn sgr_params(params: &Params) -> ParamValues {
    let mut groups: [&[u16]; 32] = [&[]; 32];
    let mut count = 0;
    for group in params {
        if count == groups.len() {
            break;
        }
        groups[count] = group;
        count += 1;
    }
    let groups = &groups[..count];
    let mut output = ParamValues::new();
    let mut index = 0;
    while let Some(group) = groups.get(index) {
        let code = group[0];
        if matches!(code, 38 | 48 | 58) {
            let mut legacy = [0_u16; 32];
            let (operands, consumed): (&[u16], usize) = if group.len() > 1 {
                (&group[1..], 0)
            } else {
                let mode = groups.get(index + 1).and_then(|g| g.first()).copied();
                let count = match mode {
                    Some(2) => 4,
                    Some(5) => 2,
                    // An unknown legacy color encoding has no known operand
                    // boundary. Ignore its remainder rather than leak styles.
                    _ => groups.len().saturating_sub(index + 1),
                };
                let consumed = count.min(groups.len().saturating_sub(index + 1));
                for (slot, g) in legacy
                    .iter_mut()
                    .zip(&groups[index + 1..index + 1 + consumed])
                {
                    *slot = if g.len() == 1 { g[0] } else { u16::MAX };
                }
                (&legacy[..consumed], consumed)
            };
            if code != 58 {
                let normalized: Option<(u16, &[u16])> = match operands {
                    [5, value] if *value <= 255 => Some((5, std::slice::from_ref(value))),
                    [2, rgb @ ..] | [2, 0, rgb @ ..]
                        if rgb.len() == 3 && rgb.iter().all(|v| *v <= 255) =>
                    {
                        Some((2, rgb))
                    }
                    _ => None,
                };
                if let Some((mode, values)) = normalized {
                    output.push(i64::from(code));
                    output.push(i64::from(mode));
                    for value in values {
                        output.push(i64::from(*value));
                    }
                }
            }
            index += consumed;
        } else if group.len() == 1 {
            output.push(i64::from(code));
        } else if let [4, kind @ 0..=5] = **group {
            output.push(if kind == 0 { 24 } else { 4 });
        }
        index += 1;
    }
    // An ignored operation is not SGR 0. The grid treats an empty slice as reset.
    if output.as_slice().is_empty() {
        output.push(-1);
    }
    output
}

fn flatten_params(params: &Params) -> ParamValues {
    let mut values = ParamValues::new();
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

#[cfg(test)]
fn trailing_incomplete_sequence(bytes: &[u8]) -> Vec<u8> {
    let mut parser = Parser::new();
    parser.advance(&mut ParserRestore, bytes);
    let mut prefix = Vec::new();
    parser.write_replay_prefix(&mut prefix);
    prefix
}

#[cfg(test)]
mod tests {
    use crate::model::{GridLimits, TerminalGrid};
    use crate::parser::TerminalGridStream;

    /// The grid's performer, printing ASCII a character at a time.
    struct Characters<'p, 'a>(&'p mut super::GridPerformer<'a, super::IgnoreSequences>);

    impl crate::vt_parser::Perform for Characters<'_, '_> {
        fn print(&mut self, character: char) {
            self.0.print(character);
        }
        fn execute(&mut self, byte: u8) {
            self.0.execute(byte);
        }
        fn hook(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
            self.0.hook(params, inter, ignore, action);
        }
        fn put(&mut self, byte: u8) {
            self.0.put(byte);
        }
        fn unhook(&mut self) {
            self.0.unhook();
        }
        fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
            self.0.osc_dispatch(params, bell);
        }
        fn csi_dispatch(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
            self.0.csi_dispatch(params, inter, ignore, action);
        }
        fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, byte: u8) {
            self.0.esc_dispatch(inter, ignore, byte);
        }
    }

    use crate::vt_parser::Params;

    /// What [`TerminalGridStream::process`] does with `chunk`, printing a character at a time.
    fn process_per_character(stream: &mut TerminalGridStream, chunk: &[u8]) {
        let revision = stream.grid.revision();
        std::mem::swap(&mut stream.pending_bytes, &mut stream.previous_pending);
        let mut performer = super::GridPerformer {
            grid: &mut stream.grid,
            observer: &mut super::IgnoreSequences,
            dcs: &mut stream.observed_dcs,
        };
        stream
            .parser
            .advance(&mut Characters(&mut performer), chunk);
        stream.parser.write_replay_prefix(&mut stream.pending_bytes);
        if stream.pending_bytes != stream.previous_pending && stream.grid.revision() == revision {
            stream.grid.bump_revision();
        }
    }

    /// Pseudo-random terminal output mixing ASCII runs with what interacts with them: wide and
    /// combining characters, moves, erases, styled blanks, insert mode, autowrap, DEC line
    /// drawing, repeats, scroll regions, tabs, backspace and DEL.
    fn mixed_stream(tokens: usize) -> Vec<u8> {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = |bound: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % bound.max(1)
        };
        let mut stream = Vec::new();
        for _ in 0..tokens {
            let token: Vec<u8> = match next(24) {
                0..=5 => b"words and more ascii text".to_vec(),
                6 => b"   ".to_vec(),
                7 => "界".as_bytes().to_vec(),
                8 => "e\u{301}x".as_bytes().to_vec(),
                9 => "👨‍👩‍👧".as_bytes().to_vec(),
                10 => b"\r\n".to_vec(),
                11 => format!("\x1b[{};{}H", next(6) + 1, next(14) + 1).into_bytes(),
                12 => format!("\x1b[{}K", next(3)).into_bytes(),
                13 => format!("\x1b[{}J", next(3)).into_bytes(),
                14 => format!("\x1b[48;5;{}m", next(256)).into_bytes(),
                15 => b"\x1b[0m".to_vec(),
                16 if next(2) == 0 => b"\x1b[4h".to_vec(),
                16 => b"\x1b[4l".to_vec(),
                17 if next(2) == 0 => b"\x1b[?7l".to_vec(),
                17 => b"\x1b[?7h".to_vec(),
                18 if next(2) == 0 => b"\x1b(0qx".to_vec(),
                18 => b"\x1b(B".to_vec(),
                19 => format!("z\x1b[{}b", next(4) + 1).into_bytes(),
                20 => format!("\x1b[{};{}r", next(3) + 1, next(3) + 3).into_bytes(),
                21 => b"\t\x08\x7f".to_vec(),
                22 => {
                    format!("\x1b[{}G{}", next(14) + 1, "界".repeat(next(3) as usize)).into_bytes()
                }
                _ => b"\x0e\x0f".to_vec(),
            };
            stream.extend(token);
        }
        stream
    }

    /// Printing a run of ASCII at once leaves the grid exactly as printing it a character at a
    /// time: cells, wraps, wide characters it overwrites, styled blanks, history, cursor,
    /// revisions and character state, at every chunk boundary.
    #[test]
    fn printing_ascii_runs_matches_printing_each_character() {
        let stream = mixed_stream(4000);
        for chunk_size in [1, 5, 13, 97, 4096] {
            let limits = GridLimits {
                scrollback_rows: 20,
            };
            let mut runs = TerminalGridStream::new(13, 5, limits).unwrap();
            let mut characters = TerminalGridStream::new(13, 5, limits).unwrap();
            for (index, chunk) in stream.chunks(chunk_size).enumerate() {
                runs.process(chunk);
                process_per_character(&mut characters, chunk);
                let (a, b) = (runs.grid(), characters.grid());
                let at = format!("chunk size {chunk_size}, chunk {index}");
                let rows = a.height() + a.history_line_count();
                assert_eq!(a.snapshot(0, rows), b.snapshot(0, rows), "{at}");
                // Rows cell for cell too: snapshots drop trailing blanks.
                assert_eq!(a.viewport_rows(), b.viewport_rows(), "{at}");
                assert_eq!(a.cursor(), b.cursor(), "{at}");
                assert_eq!(a.revision(), b.revision(), "{at}");
                assert_eq!(a.content_revision(), b.content_revision(), "{at}");
                assert_eq!(a.characters, b.characters, "{at}");
            }
        }
    }

    /// The protocol tracker skips text (its performer ignores it): what it tracks after every
    /// chunk is what parsing every byte tracks, through UTF-8 split across chunks and interrupted
    /// by escapes, mode changes, synchronized updates, pointer-shape strings, CAN/SUB and raw C1.
    #[test]
    fn skipping_text_tracks_the_protocol_exactly_as_parsing_every_byte() {
        use super::{ProtocolPerformer, TerminalProtocolTracker};
        use crate::vt_parser::{Params, Perform};

        /// The protocol performer, observing text like any other performer.
        struct Observing<'p, 'a>(&'p mut ProtocolPerformer<'a>);
        impl Perform for Observing<'_, '_> {
            fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
                self.0.osc_dispatch(params, bell);
            }
            fn csi_dispatch(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
                self.0.csi_dispatch(params, inter, ignore, action);
            }
            fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, byte: u8) {
                self.0.esc_dispatch(inter, ignore, byte);
            }
        }
        fn observe(tracker: &mut TerminalProtocolTracker, bytes: &[u8]) -> (bool, bool) {
            let mut performer = ProtocolPerformer {
                protocol: &mut tracker.protocol,
                alternate_screen: &mut tracker.alternate_screen,
                synchronized_update: &mut tracker.synchronized_update,
                toggled_alternate: false,
                ended_synchronized_update: false,
            };
            tracker
                .parser
                .advance(&mut Observing(&mut performer), bytes);
            (
                performer.toggled_alternate,
                performer.ended_synchronized_update,
            )
        }

        let mut stream = Vec::new();
        for round in 0..40_u32 {
            stream.extend_from_slice("héllo wörld 界 👨‍👩‍👧 ".as_bytes());
            stream.extend_from_slice(match round % 10 {
                0 => b"\x1b[?1049h\x1b[?1000;1006h".as_slice(),
                1 => b"\x1b[?2026h\x1b[1;31mframe\x1b[0m\x1b[?2026l",
                2 => b"\x1b]22;pointer\x1b\\\x1b]22;>crosshair\x07",
                3 => b"\x1b[?1h\x1b=\x1b[?2004h",
                4 => b"\xe4\xb8\x1b[?1049l\xad",
                5 => b"\x1b[?25\x18l\x1b[?1002h\x1a",
                6 => b"\x9b?1049h\xc2\x9b\x1b[?1003l",
                7 => b"\x1bP1;2|data\x1b\\\x1b_apc\x1b\\",
                8 => b"\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[\x07?1049h\x1b[\x7f?1049l\x1b[1?h\x1b[>1h\x1b[?$p\x1b[12;24H\x1b[2J",
                _ => b"\r\n\x08\x07\t",
            });
        }
        for chunk_size in [1, 2, 3, 7, 64, 4096] {
            let (mut skipping, mut observing) = (
                TerminalProtocolTracker::new(),
                TerminalProtocolTracker::new(),
            );
            let (mut toggles, mut ends) = (0, 0);
            for chunk in stream.chunks(chunk_size) {
                let outcome = skipping.process(chunk);
                toggles += usize::from(outcome.toggled_alternate);
                ends += usize::from(outcome.ended_synchronized_update);
                let expected = observe(&mut observing, chunk);
                assert_eq!(
                    (outcome.toggled_alternate, outcome.ended_synchronized_update),
                    expected,
                    "chunk size {chunk_size}"
                );
                assert_eq!(skipping.protocol_state(), observing.protocol_state());
                assert_eq!(skipping.alternate_screen(), observing.alternate_screen());
                assert_eq!(
                    skipping.synchronized_update(),
                    observing.synchronized_update()
                );
            }
            assert!(toggles > 0 && ends > 0, "the stream exercises the protocol");
        }
    }

    #[test]
    fn normal_text_keeps_protocol_prefix_storage_unallocated() {
        let bytes = [b'x'; 8192];
        for chunk_size in [1, 32, 8192] {
            let mut stream =
                TerminalGridStream::new(80, 2, GridLimits { scrollback_rows: 0 }).unwrap();
            let mut tracker = super::TerminalProtocolTracker::new();
            for chunk in bytes.chunks(chunk_size) {
                stream.process(chunk);
                tracker.process(chunk);
                assert_eq!(stream.pending_bytes.len(), 0);
                assert_eq!(tracker.pending_bytes.len(), 0);
                // Protocol prefix capacity only, not allocator-wide counts.
                assert_eq!(tracker.pending_bytes.capacity(), 0);
                assert_eq!(stream.pending_bytes.capacity(), 0);
            }
        }
    }

    #[test]
    fn repeated_grid_prefixes_preserve_pending_bytes() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        for _ in 0..32 {
            stream.process(b"\x1b[3");
            assert_eq!(stream.pending_bytes, b"\x1b[3");
            stream.process(b"1");
            assert_eq!(stream.pending_bytes, b"\x1b[31");
            stream.process(b"m");
            assert_eq!(stream.pending_bytes.len(), 0);
        }
    }

    #[test]
    fn prefix_only_changes_preserve_revision_and_hydration_semantics() {
        let limits = GridLimits::default();
        let mut stream = TerminalGridStream::new(10, 2, limits).unwrap();
        let revision = stream.grid().revision();
        let content_revision = stream.grid().content_revision();
        stream.process(b"\x1b[");
        assert_eq!(stream.grid().revision(), revision + 1);
        assert_eq!(stream.grid().content_revision(), content_revision);
        // NUL executes no grid operation and does not change the CSI prefix.
        stream.process(b"\0");
        assert_eq!(stream.grid().revision(), revision + 1);
        stream.process(b"3");
        assert_eq!(stream.grid().revision(), revision + 2);
        assert_eq!(stream.grid().content_revision(), content_revision);
        let snapshot = stream.snapshot(0, 2);
        assert_eq!(snapshot.pending_bytes, b"\x1b[3");
        let mut hydrated = TerminalGridStream::from_snapshot(&snapshot, limits).unwrap();
        for chunk in [&b"1m"[..], &b"X"[..]] {
            stream.process(chunk);
            hydrated.process(chunk);
            assert_eq!(hydrated.snapshot(0, 2), stream.snapshot(0, 2));
        }
        assert_eq!(stream.pending_bytes.len(), 0);
    }

    #[test]
    fn clearing_prefix_without_visible_work_advances_only_replication() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        stream.process(b"\x1b[3");
        let revision = stream.grid().revision();
        let content_revision = stream.grid().content_revision();
        // CAN discards the pending sequence without changing visible cells.
        stream.process(b"\x18");
        assert_eq!(stream.pending_bytes.len(), 0);
        assert_eq!(stream.grid().revision(), revision + 1);
        assert_eq!(stream.grid().content_revision(), content_revision);
        stream.process(b"");
        assert_eq!(stream.grid().revision(), revision + 1);
    }

    #[test]
    fn protocol_tracker_reuses_prefix_storage() {
        let mut tracker = super::TerminalProtocolTracker::new();
        tracker.process(b"\x1b[?100");
        let pointer = tracker.pending_bytes.as_ptr();
        let capacity = tracker.pending_bytes.capacity();
        for _ in 0..32 {
            tracker.process(b"0h");
            assert_eq!(tracker.pending_bytes.len(), 0);
            tracker.process(b"\x1b[?100");
            assert_eq!(tracker.pending_bytes, b"\x1b[?100");
            assert_eq!(tracker.pending_bytes.as_ptr(), pointer);
            assert_eq!(tracker.pending_bytes.capacity(), capacity);
        }
    }

    #[test]
    fn budgeted_prefix_capture_preserves_pending_sequence_on_rejection() {
        let mut stream = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        stream.process(b"hello\x1b[31");
        let expected = stream.snapshot(0, 2);
        assert_ne!(expected.pending_bytes, [] as [u8; 0]);
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
        assert_eq!(stream.snapshot(0, 2).pending_bytes, [] as [u8; 0]);
    }

    #[test]
    fn scrolling_a_region_by_n_lines_equals_n_single_line_scrolls() {
        let screen = |bytes: &[u8], alternate: bool| {
            let mut stream =
                crate::TerminalGridStream::new(12, 8, crate::GridLimits::default()).unwrap();
            if alternate {
                stream.process(b"\x1b[?1049h");
            }
            for row in 1..=8 {
                stream.process(format!("\x1b[{row};1Hline {row}").as_bytes());
            }
            stream.process(b"\x1b[2;7r");
            stream.process(bytes);
            crate::render::visible_text(stream.grid(), 0, 8)
        };
        for alternate in [false, true] {
            for (many, one) in [
                (&b"\x1b[3S"[..], &b"\x1b[S\x1b[S\x1b[S"[..]),
                (b"\x1b[3T", b"\x1b[T\x1b[T\x1b[T"),
                (b"\x1b[9S", b"\x1b[6S"),
                (b"\x1b[3;1H\x1b[2M", b"\x1b[3;1H\x1b[M\x1b[M"),
                (b"\x1b[3;1H\x1b[2L", b"\x1b[3;1H\x1b[L\x1b[L"),
            ] {
                let scrolled = screen(many, alternate);
                assert_eq!(
                    scrolled,
                    screen(one, alternate),
                    "{many:?} alternate={alternate}"
                );
            }
            // Rows outside the region stay; the region moved and its exposed rows are blank.
            let scrolled = screen(b"\x1b[2S", alternate);
            let lines: Vec<&str> = scrolled.lines().map(str::trim_end).collect();
            assert_eq!(lines[0], "line 1");
            assert_eq!(lines[1], "line 4");
            assert_eq!(lines[4], "line 7");
            assert_eq!(lines[5], "");
            assert_eq!(lines[7], "line 8");
        }
    }

    #[test]
    fn the_grid_reports_how_its_rows_moved_since_last_asked() {
        use crate::ViewportScroll;
        let mut stream =
            crate::TerminalGridStream::new(10, 8, crate::GridLimits::default()).unwrap();
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::None
        );
        stream.process(b"\x1b[2;7r\x1b[3S\x1b[S\x1b[T");
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::Region {
                top: 1,
                bottom: 6,
                rows: 3
            }
        );
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::None
        );
        // A linefeed at the bottom of the full screen scrolls it.
        stream.process(b"\x1b[r\x1b[8;1H\n\n");
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::Region {
                top: 0,
                bottom: 7,
                rows: 2
            }
        );
        // Two regions, a screen switch, or a resize cannot be replayed as one scroll.
        stream.process(b"\x1b[2;4r\x1b[S\x1b[5;7r\x1b[S");
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::Unknown
        );
        stream.process(b"\x1b[?1049h");
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::Unknown
        );
        stream.resize(10, 8).unwrap();
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::None
        );
        stream.resize(12, 8).unwrap();
        assert_eq!(
            stream.grid_mut().take_viewport_scroll(),
            ViewportScroll::Unknown
        );
    }

    #[test]
    fn protocol_tracker_follows_synchronized_updates_across_chunks() {
        let mut tracker = crate::TerminalProtocolTracker::new();
        assert!(!tracker.synchronized_update());
        let outcome = tracker.process(b"\x1b[?2026hframe one");
        assert!(tracker.synchronized_update() && !outcome.ended_synchronized_update);
        // The end arrives split across chunks.
        assert!(!tracker.process(b"\x1b[?20").ended_synchronized_update);
        assert!(tracker.synchronized_update());
        let outcome = tracker.process(b"26l");
        assert!(outcome.ended_synchronized_update && !tracker.synchronized_update());
        // A frame ending and the next beginning in one chunk: it ended, and one is open again.
        let outcome = tracker.process(b"\x1b[?2026hA\x1b[?2026l\x1b[?2026hB");
        assert!(outcome.ended_synchronized_update && tracker.synchronized_update());
        // A full reset closes it.
        assert!(tracker.process(b"\x1bc").ended_synchronized_update);
        assert!(!tracker.synchronized_update());
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
        assert_eq!(rows[0].cells(), []);
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

        assert_eq!(grid.viewport_rows()[0].cells(), []);
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
            assert_eq!(producer.snapshot(0, 3).pending_bytes, [] as [u8; 0]);
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
            assert_eq!(super::trailing_incomplete_sequence(invalid), [] as [u8; 0]);
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
                assert_eq!(super::trailing_incomplete_sequence(&bytes), [] as [u8; 0]);
                bytes.extend_from_slice(b"\xf0\x9f\x98");
                assert_eq!(super::trailing_incomplete_sequence(&bytes), b"\xf0\x9f\x98");
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
    fn retained_main_position_mapping_includes_pending_and_live_rows() {
        let mut stream =
            super::TerminalGridStream::new(4, 2, crate::GridLimits::default()).unwrap();
        stream.process(b"abcdefghijkl");
        let grid = stream.grid();
        assert_eq!(
            grid.main_position_at_width(0, 0, 8, &mut 100_000).unwrap(),
            (0, 0)
        );
        assert_eq!(
            grid.main_position_at_width(1, 0, 8, &mut 100_000).unwrap(),
            (0, 4)
        );
        assert_eq!(
            grid.main_position_at_width(2, 0, 8, &mut 100_000).unwrap(),
            (1, 0)
        );
        assert!(grid.main_position_at_width(2, 0, 8, &mut 1).is_err());
        assert!(grid.main_position_at_width(3, 0, 8, &mut 100_000).is_err());
    }

    #[test]
    fn captured_history_position_reflows_with_text_and_rejects_wide_interiors() {
        let mut stream =
            super::TerminalGridStream::new(4, 2, crate::GridLimits::default()).unwrap();
        stream.process("ab界cd\r\nx\r\ny\r\nz\r\n".as_bytes());
        let grid = stream.grid();
        assert_eq!(
            grid.history_position_at_width(1, 0, 8, &mut 1000).unwrap(),
            (0, 4)
        );
        assert_eq!(
            grid.history_position_at_width(0, 2, 3, &mut 1000).unwrap(),
            (1, 0)
        );
        assert!(grid.history_position_at_width(0, 3, 3, &mut 1000).is_err());
        assert!(grid.history_position_at_width(1, 0, 8, &mut 0).is_err());
        assert!(grid.history_position_at_width(1, 0, 0, &mut 1000).is_err());
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
    fn explicit_scroll_commands_preserve_rows_outside_margins() {
        for (command, expected) in [
            (b'S', ["HUD", "three", "four", "", "", "FOOT"]),
            (b'T', ["HUD", "", "", "one", "two", "FOOT"]),
        ] {
            let mut stream = TerminalGridStream::new(12, 6, GridLimits::default()).unwrap();
            stream.process(b"HUD\r\none\r\ntwo\r\nthree\r\nfour\r\nFOOT");
            stream.process(b"\x1b[2;5r\x1b[4;7H");
            let cursor = stream.grid().cursor();
            // Split like real PTY chunks; scroll only rows 2..5, not the HUD.
            for byte in [0x1b, b'[', b'2', command] {
                stream.process(&[byte]);
            }
            for (row, text) in stream.grid().viewport_rows().iter().zip(expected) {
                assert_eq!(row_text(row), text);
            }
            assert_eq!(stream.grid().cursor(), cursor);
        }
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

    /// DECSTR returns every input mode to its default in the grid and the protocol tracker alike,
    /// keeping the pointer shape; parameters or other intermediates make it something else.
    #[test]
    fn soft_reset_returns_input_modes_to_defaults() {
        let modes = b"\x1b[?1003h\x1b[?1006h\x1b[?1;2004h\x1b=\x1b]22;pointer\x1b\\";
        let mut stream = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        let mut tracker = super::TerminalProtocolTracker::new();
        stream.process(modes);
        tracker.process(modes);
        let shapes = stream.grid().protocol_state().pointer_shapes;
        for not_reset in [&b"\x1b[1!p"[..], b"\x1b[?!p", b"\x1b[\"p"] {
            stream.process(not_reset);
            tracker.process(not_reset);
            assert!(stream.grid().protocol_state().bracketed_paste);
            assert!(tracker.protocol_state().bracketed_paste);
        }
        let revision = stream.grid().revision();
        stream.process(b"\x1b[!p");
        tracker.process(b"\x1b[!p");
        let expected = crate::ProtocolState {
            pointer_shapes: shapes,
            ..crate::ProtocolState::default()
        };
        assert_eq!(stream.grid().protocol_state(), expected);
        assert_eq!(tracker.protocol_state(), expected);
        assert!(stream.grid().revision() > revision);
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
