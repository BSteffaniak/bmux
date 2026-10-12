//! Private, bounded terminal syntax decoder.
//!
//! The control-state organization follows Paul Williams' DEC ANSI parser
//! (<https://vt100.net/emu/dec_ansi_parser>). UTF-8 text and raw C1 handling follow
//! the existing byte-stream contract rather than enabling eight-bit introducers.
//! Storage is fixed-size; string payload overflow discards only excess bytes.

#[derive(Debug)]
pub(crate) struct Params {
    values: [u16; 32],
    ends: [usize; 32],
    used: usize,
    groups: usize,
}

impl Params {
    const fn new() -> Self {
        Self {
            values: [0; 32],
            ends: [0; 32],
            used: 0,
            groups: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.groups
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.groups == 0
    }
    pub(crate) fn iter(&self) -> ParamsIter<'_> {
        ParamsIter {
            params: self,
            group: 0,
            start: 0,
        }
    }

    fn clear(&mut self) {
        self.used = 0;
        self.groups = 0;
        self.ends.fill(0);
    }

    // A colon extends the current group; a semicolon or final closes it.
    fn append(&mut self, value: u16, close: bool) -> bool {
        if self.used == self.values.len() {
            return false;
        }
        self.values[self.used] = value;
        self.used += 1;
        self.ends[self.groups] = self.used;
        if close {
            self.groups += 1;
        }
        true
    }

    fn finish(&mut self, value: u16) -> bool {
        if self.append(value, true) {
            return true;
        }
        // If overflow occurred within a colon group, expose its stored prefix.
        if self.groups < self.ends.len() && self.ends[self.groups] == self.used {
            self.groups += 1;
        }
        false
    }
}

pub(crate) struct ParamsIter<'a> {
    params: &'a Params,
    group: usize,
    start: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];
    fn next(&mut self) -> Option<Self::Item> {
        if self.params.is_empty() || self.group == self.params.len() {
            return None;
        }
        let end = self.params.ends[self.group];
        let result = &self.params.values[self.start..end];
        self.start = end;
        self.group += 1;
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.params.len() - self.group;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for ParamsIter<'_> {}
impl<'a> IntoIterator for &'a Params {
    type Item = &'a [u16];
    type IntoIter = ParamsIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub(crate) trait Perform {
    fn print(&mut self, _character: char) {}
    fn execute(&mut self, _byte: u8) {}
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    Escape,
    EscapeCollect,
    CsiStart,
    CsiNumbers,
    CsiCollect,
    CsiDiscard,
    DcsStart,
    DcsNumbers,
    DcsCollect,
    DcsDiscard,
    DcsData,
    Osc,
    StringDiscard,
}

pub(crate) struct Parser<const OSC_RAW_BUF_SIZE: usize = 1024> {
    state: State,
    params: Params,
    number: u16,
    intermediates: [u8; 2],
    intermediate_len: usize,
    ignored: bool,
    osc: [u8; OSC_RAW_BUF_SIZE],
    osc_len: usize,
    osc_ends: [usize; 16],
    osc_args: usize,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_expected: usize,
}

impl Parser {
    pub(crate) fn new() -> Self {
        Self::new_with_size()
    }
}

impl<const N: usize> Parser<N> {
    pub(crate) fn new_with_size() -> Self {
        Self {
            state: State::Text,
            params: Params::new(),
            number: 0,
            intermediates: [0; 2],
            intermediate_len: 0,
            ignored: false,
            osc: [0; N],
            osc_len: 0,
            osc_ends: [0; 16],
            osc_args: 0,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_expected: 0,
        }
    }

    pub(crate) fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut offset = 0;
        if self.utf8_len != 0 {
            let count = (4 - self.utf8_len).min(bytes.len());
            self.utf8[self.utf8_len..self.utf8_len + count].copy_from_slice(&bytes[..count]);
            self.utf8_len += count;
            offset = self.complete_saved_utf8(performer, count);
        }
        for &byte in &bytes[offset..] {
            self.consume(performer, byte);
        }
    }

    /// A bounded, normalized syntax prefix for semantic restoration.
    ///
    /// Feed this to a fresh decoder with a no-op performer. Completed callbacks
    /// must not be replayed into an application. OSC/DCS payloads are deliberately
    /// omitted: this restores syntax continuation, not a callback recording.
    /// Numeric groups and ignore flags are retained so a later CSI dispatch has
    /// the same meaning. The prefix is at most 208 bytes, independent of input.
    pub(crate) fn replay_prefix(&self) -> Vec<u8> {
        let mut prefix = Vec::with_capacity(208);
        match self.state {
            State::Text => prefix.extend_from_slice(&self.utf8[..self.utf8_len]),
            State::Osc => prefix.extend_from_slice(b"\x1b]"),
            State::StringDiscard => prefix.extend_from_slice(b"\x1b_"),
            State::DcsData => prefix.extend_from_slice(b"\x1bPq"),
            State::DcsDiscard => prefix.extend_from_slice(b"\x1bP $0"),
            State::CsiDiscard => prefix.extend_from_slice(b"\x1b[ $0"),
            State::Escape | State::EscapeCollect => {
                prefix.push(0x1b);
                prefix.extend_from_slice(&self.intermediates[..self.intermediate_len]);
                if self.ignored {
                    prefix.push(b' ');
                }
            }
            State::CsiStart
            | State::CsiNumbers
            | State::CsiCollect
            | State::DcsStart
            | State::DcsNumbers
            | State::DcsCollect => {
                let dcs = matches!(
                    self.state,
                    State::DcsStart | State::DcsNumbers | State::DcsCollect
                );
                prefix.extend_from_slice(if dcs { b"\x1bP" } else { b"\x1b[" });
                let intermediates = &self.intermediates[..self.intermediate_len];
                // A private marker precedes numeric parameters, while ordinary
                // intermediates follow them. Their stored order is significant.
                let private = usize::from(intermediates.first().is_some_and(|byte| *byte >= 0x3c));
                prefix.extend_from_slice(&intermediates[..private]);
                let mut group = 0;
                for index in 0..self.params.used {
                    Self::write_number(&mut prefix, self.params.values[index]);
                    let closed = group < self.params.groups && self.params.ends[group] == index + 1;
                    prefix.push(if closed { b';' } else { b':' });
                    if closed {
                        group += 1;
                    }
                }
                if self.params.used != 0
                    || self.number != 0
                    || matches!(self.state, State::CsiNumbers | State::DcsNumbers)
                {
                    Self::write_number(&mut prefix, self.number);
                }
                if self.ignored && self.params.used == 32 {
                    prefix.push(b'0');
                }
                prefix.extend_from_slice(&intermediates[private..]);
                if self.ignored && self.params.used < 32 {
                    // Overflow of the two-byte intermediate storage. The first
                    // two bytes remain unchanged and excess bytes are ignored.
                    prefix.push(b' ');
                }
            }
        }
        prefix
    }

    fn write_number(prefix: &mut Vec<u8>, value: u16) {
        let mut digits = [0; 5];
        let mut index = digits.len();
        let mut remaining = value;
        loop {
            index -= 1;
            // The remainder is bounded by nine.
            digits[index] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        prefix.extend_from_slice(&digits[index..]);
    }

    // Completing saved text deliberately uses a bounded lookahead. The pinned
    // byte-stream contract prints one saved scalar, reprocesses malformed bytes,
    // and consumes a valid prefix before an error even if it contains more than
    // one scalar. Do not normalize this into chunk-independent UTF-8 decoding.
    fn complete_saved_utf8<P: Perform>(&mut self, performer: &mut P, added: usize) -> usize {
        let original = self.utf8_len - added;
        match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
            Ok(text) => {
                let length = text.chars().next().map_or(0, |character| {
                    performer.print(character);
                    character.len_utf8()
                });
                self.utf8_len = 0;
                length.saturating_sub(original)
            }
            Err(error) if error.valid_up_to() != 0 => {
                let end = error.valid_up_to();
                if let Ok(text) = std::str::from_utf8(&self.utf8[..end])
                    && let Some(character) = text.chars().next()
                {
                    performer.print(character);
                }
                self.utf8_len = 0;
                end.saturating_sub(original)
            }
            Err(error) => {
                if let Some(length) = error.error_len() {
                    performer.print('\u{fffd}');
                    self.utf8_len = 0;
                    length.saturating_sub(original)
                } else {
                    added
                }
            }
        }
    }

    fn clear_header(&mut self) {
        self.params.clear();
        self.number = 0;
        self.intermediate_len = 0;
        self.ignored = false;
    }

    fn collect(&mut self, byte: u8) {
        if self.intermediate_len < self.intermediates.len() {
            self.intermediates[self.intermediate_len] = byte;
            self.intermediate_len += 1;
        } else {
            self.ignored = true;
        }
    }

    fn parameter(&mut self, byte: u8) {
        if self.params.used == 32 {
            self.ignored = true;
            return;
        }
        match byte {
            b'0'..=b'9' => {
                self.number = self
                    .number
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte - b'0'));
            }
            b':' | b';' => {
                self.params.append(self.number, byte == b';');
                self.number = 0;
            }
            _ => {}
        }
    }

    fn finish_header(&mut self) {
        if !self.params.finish(self.number) {
            self.ignored = true;
        }
    }

    fn end_string<P: Perform>(&mut self, performer: &mut P, bell: bool) {
        match self.state {
            State::DcsData => performer.unhook(),
            State::Osc => {
                if self.osc_args < 16 {
                    self.osc_ends[self.osc_args] = self.osc_len;
                    self.osc_args += 1;
                }
                let mut args: [&[u8]; 16] = [&[]; 16];
                let mut start = 0;
                for (index, arg) in args.iter_mut().enumerate().take(self.osc_args) {
                    let end = self.osc_ends[index];
                    *arg = &self.osc[start..end];
                    start = end;
                }
                performer.osc_dispatch(&args[..self.osc_args], bell);
            }
            _ => {}
        }
    }

    fn consume<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.state == State::Text && self.utf8_len != 0 && self.continue_utf8(performer, byte) {
            return;
        }
        // These interruptions run exit callbacks before the incoming action.
        match byte {
            0x18 | 0x1a => {
                self.end_string(performer, false);
                performer.execute(byte);
                self.state = State::Text;
                return;
            }
            0x1b => {
                self.end_string(performer, false);
                self.clear_header();
                self.state = State::Escape;
                return;
            }
            _ => {}
        }
        match self.state {
            State::Text => self.text(performer, byte),
            State::Escape | State::EscapeCollect => self.escape(performer, byte),
            State::CsiStart
            | State::CsiNumbers
            | State::CsiCollect
            | State::DcsStart
            | State::DcsNumbers
            | State::DcsCollect => self.header(performer, byte),
            State::CsiDiscard => match byte {
                0x00..=0x1f => performer.execute(byte),
                0x40..=0x7e => self.state = State::Text,
                _ => {}
            },
            State::DcsData => match byte {
                0x9c => {
                    performer.unhook();
                    self.state = State::Text;
                }
                0x00..=0x7e => performer.put(byte),
                _ => {}
            },
            State::Osc => match byte {
                0x07 => {
                    self.end_string(performer, true);
                    self.state = State::Text;
                }
                0x00..=0x1f => {}
                b';' => {
                    if self.osc_args < 16 {
                        self.osc_ends[self.osc_args] = self.osc_len;
                        self.osc_args += 1;
                    }
                }
                _ => {
                    if self.osc_len < N && self.osc_args < 16 {
                        self.osc[self.osc_len] = byte;
                        self.osc_len += 1;
                    }
                }
            },
            State::DcsDiscard => {
                if byte == 0x9c {
                    self.state = State::Text;
                }
            }
            State::StringDiscard => {}
        }
    }

    fn escape<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x1f => performer.execute(byte),
            0x20..=0x2f => {
                self.collect(byte);
                self.state = State::EscapeCollect;
            }
            b'[' if self.state == State::Escape => {
                self.clear_header();
                self.state = State::CsiStart;
            }
            b'P' if self.state == State::Escape => {
                self.clear_header();
                self.state = State::DcsStart;
            }
            b']' if self.state == State::Escape => {
                self.osc_len = 0;
                self.osc_args = 0;
                self.state = State::Osc;
            }
            b'X' | b'^' | b'_' if self.state == State::Escape => self.state = State::StringDiscard,
            0x30..=0x7e => {
                performer.esc_dispatch(
                    &self.intermediates[..self.intermediate_len],
                    self.ignored,
                    byte,
                );
                self.state = State::Text;
            }
            _ => {}
        }
    }

    fn header<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        let dcs = matches!(
            self.state,
            State::DcsStart | State::DcsNumbers | State::DcsCollect
        );
        let collecting = matches!(self.state, State::CsiCollect | State::DcsCollect);
        let entry = matches!(self.state, State::CsiStart | State::DcsStart);
        match byte {
            0x00..=0x1f => {
                if !dcs {
                    performer.execute(byte);
                }
            }
            0x20..=0x2f => {
                self.collect(byte);
                self.state = if dcs {
                    State::DcsCollect
                } else {
                    State::CsiCollect
                };
            }
            0x30..=0x3f => {
                if collecting || (byte >= 0x3c && !entry) {
                    self.state = if dcs {
                        State::DcsDiscard
                    } else {
                        State::CsiDiscard
                    };
                } else if byte >= 0x3c {
                    self.collect(byte);
                    self.state = if dcs {
                        State::DcsNumbers
                    } else {
                        State::CsiNumbers
                    };
                } else {
                    self.parameter(byte);
                    self.state = if dcs {
                        State::DcsNumbers
                    } else {
                        State::CsiNumbers
                    };
                }
            }
            0x40..=0x7e => {
                self.finish_header();
                let intermediates = &self.intermediates[..self.intermediate_len];
                if dcs {
                    performer.hook(&self.params, intermediates, self.ignored, char::from(byte));
                    self.state = State::DcsData;
                } else {
                    performer.csi_dispatch(
                        &self.params,
                        intermediates,
                        self.ignored,
                        char::from(byte),
                    );
                    self.state = State::Text;
                }
            }
            _ => {}
        }
    }

    fn text<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x1f | 0x80..=0x9f => performer.execute(byte),
            0x20..=0x7f => performer.print(char::from(byte)),
            0xc2..=0xf4 => {
                self.utf8[0] = byte;
                self.utf8_len = 1;
                self.utf8_expected = if byte < 0xe0 {
                    2
                } else if byte < 0xf0 {
                    3
                } else {
                    4
                };
            }
            _ => performer.print('\u{fffd}'),
        }
    }

    // Reject overlong forms, surrogates and values beyond Unicode before
    // accepting their second byte. On failure reprocess the offending byte.
    fn continue_utf8<P: Perform>(&mut self, performer: &mut P, byte: u8) -> bool {
        let first = self.utf8[0];
        let valid = (0x80..=0xbf).contains(&byte)
            && (self.utf8_len != 1
                || match first {
                    0xe0 => byte >= 0xa0,
                    0xed => byte <= 0x9f,
                    0xf0 => byte >= 0x90,
                    0xf4 => byte <= 0x8f,
                    _ => true,
                });
        if !valid {
            self.utf8_len = 0;
            performer.print('\u{fffd}');
            return false;
        }
        self.utf8[self.utf8_len] = byte;
        self.utf8_len += 1;
        if self.utf8_len == self.utf8_expected {
            if let Ok(text) = std::str::from_utf8(&self.utf8[..self.utf8_len])
                && let Some(character) = text.chars().next()
            {
                if ('\u{80}'..='\u{9f}').contains(&character) {
                    // A UTF-8 encoded C1 is a control too, not printable text.
                    if let Ok(control) = u8::try_from(u32::from(character)) {
                        performer.execute(control);
                    }
                } else {
                    performer.print(character);
                }
            }
            self.utf8_len = 0;
        }
        true
    }
}
