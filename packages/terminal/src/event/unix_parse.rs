//! Decoding of terminal input bytes into events.
//!
//! Bytes are accumulated one at a time. After each byte the buffer is
//! classified as a complete event, an incomplete prefix that needs more bytes,
//! or an unrecognizable sequence that is discarded. A lone ESC is reported as
//! the Escape key when it is the last byte currently available.

use std::collections::VecDeque;

use super::reader::InternalEvent;
use super::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode,
    ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
};

/// Outcome of classifying the accumulated bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A complete event; the buffer is consumed.
    Event(InternalEvent),
    /// A valid prefix; keep the buffer and wait for more bytes.
    Incomplete,
    /// Not a recognizable sequence; the buffer is discarded.
    Invalid,
}

use Parsed::{Incomplete, Invalid};

/// Incremental decoder state.
#[derive(Debug)]
pub struct Parser {
    buffer: Vec<u8>,
    events: VecDeque<InternalEvent>,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(256),
            events: VecDeque::with_capacity(128),
        }
    }
}

impl Parser {
    /// Decode `bytes`. `more` reports whether further bytes are already
    /// available beyond this chunk.
    pub fn advance(&mut self, bytes: &[u8], more: bool, raw_mode: bool) {
        for (index, byte) in bytes.iter().enumerate() {
            let more = more || index + 1 < bytes.len();
            self.buffer.push(*byte);
            match parse(&self.buffer, more, raw_mode) {
                Parsed::Event(event) => {
                    self.events.push_back(event);
                    self.buffer.clear();
                }
                Incomplete => {}
                Invalid => self.buffer.clear(),
            }
        }
    }

    /// The next decoded event.
    pub fn next(&mut self) -> Option<InternalEvent> {
        self.events.pop_front()
    }
}

const fn key(event: KeyEvent) -> Parsed {
    Parsed::Event(InternalEvent::Event(Event::Key(event)))
}

const fn plain(code: KeyCode) -> Parsed {
    key(KeyEvent::new(code, KeyModifiers::NONE))
}

const fn ctrl(character: char) -> Parsed {
    key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL))
}

const fn event(event: Event) -> Parsed {
    Parsed::Event(InternalEvent::Event(event))
}

/// Classify the accumulated bytes.
pub fn parse(buffer: &[u8], more: bool, raw_mode: bool) -> Parsed {
    let Some(&first) = buffer.first() else {
        return Incomplete;
    };
    match first {
        0x1b => parse_escape(buffer, more, raw_mode),
        b'\r' => plain(KeyCode::Enter),
        // In raw mode a line feed can only come from Ctrl+J; outside raw mode
        // the terminal translated Enter's carriage return into it.
        b'\n' if !raw_mode => plain(KeyCode::Enter),
        b'\t' => plain(KeyCode::Tab),
        0x7f => plain(KeyCode::Backspace),
        0x01..=0x1a => ctrl(char::from(first - 0x01 + b'a')),
        0x1c..=0x1f => ctrl(char::from(first - 0x1c + b'4')),
        0x00 => ctrl(' '),
        _ => match parse_utf8_char(buffer) {
            Ok(Some(character)) => {
                let modifiers = if character.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                key(KeyEvent::new(KeyCode::Char(character), modifiers))
            }
            Ok(None) => Incomplete,
            Err(()) => Invalid,
        },
    }
}

fn parse_escape(buffer: &[u8], more: bool, raw_mode: bool) -> Parsed {
    let Some(&second) = buffer.get(1) else {
        // A lone ESC starts a sequence only if more bytes are on the way.
        return if more {
            Incomplete
        } else {
            plain(KeyCode::Esc)
        };
    };
    match second {
        b'O' => {
            let Some(&third) = buffer.get(2) else {
                return Incomplete;
            };
            match third {
                b'D' => plain(KeyCode::Left),
                b'C' => plain(KeyCode::Right),
                b'A' => plain(KeyCode::Up),
                b'B' => plain(KeyCode::Down),
                b'H' => plain(KeyCode::Home),
                b'F' => plain(KeyCode::End),
                b'P'..=b'S' => plain(KeyCode::F(1 + third - b'P')),
                _ => Invalid,
            }
        }
        b'[' => parse_csi(buffer, raw_mode),
        0x1b => plain(KeyCode::Esc),
        // ESC before anything else is Alt applied to what follows.
        _ => match parse(&buffer[1..], more, raw_mode) {
            Parsed::Event(InternalEvent::Event(Event::Key(mut key_event))) => {
                key_event.modifiers.insert(KeyModifiers::ALT);
                key(key_event)
            }
            other => other,
        },
    }
}

fn parse_csi(buffer: &[u8], raw_mode: bool) -> Parsed {
    let Some(&third) = buffer.get(2) else {
        return Incomplete;
    };
    match third {
        // Linux console function keys: ESC [ [ A..E.
        b'[' => match buffer.get(3) {
            None => Incomplete,
            Some(&letter @ b'A'..=b'E') => plain(KeyCode::F(1 + letter - b'A')),
            Some(_) => Invalid,
        },
        b'D' => plain(KeyCode::Left),
        b'C' => plain(KeyCode::Right),
        b'A' => plain(KeyCode::Up),
        b'B' => plain(KeyCode::Down),
        b'H' => plain(KeyCode::Home),
        b'F' => plain(KeyCode::End),
        b'Z' => key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        b'M' => parse_x10_mouse(buffer),
        b'<' => parse_sgr_mouse(buffer),
        b'I' => event(Event::FocusGained),
        b'O' => event(Event::FocusLost),
        b';' => parse_modified_key(buffer),
        // Kitty reports unmodified F1, F2, and F4 without the leading `1`.
        b'P' => plain(KeyCode::F(1)),
        b'Q' => plain(KeyCode::F(2)),
        b'S' => plain(KeyCode::F(4)),
        b'?' => match buffer.last() {
            Some(b'u') if buffer.len() >= 5 => {
                Parsed::Event(InternalEvent::KeyboardEnhancementFlags)
            }
            Some(b'c') => Parsed::Event(InternalEvent::PrimaryDeviceAttributes),
            _ => Incomplete,
        },
        b'0'..=b'9' => {
            if buffer.len() == 3 {
                return Incomplete;
            }
            let last = buffer[buffer.len() - 1];
            // Parameter and intermediate bytes continue the sequence.
            if !(0x40..=0x7e).contains(&last) {
                return Incomplete;
            }
            if buffer.starts_with(b"\x1b[200~") {
                return parse_bracketed_paste(buffer);
            }
            match last {
                b'M' => parse_rxvt_mouse(buffer),
                b'~' => parse_tilde_key(buffer),
                b'u' => parse_csi_u(buffer, raw_mode),
                b'R' => parse_cursor_position(buffer),
                _ => parse_modified_key(buffer),
            }
        }
        _ => Invalid,
    }
}

/// The parameter bytes between `ESC [` and the final byte, as text.
fn parameters(buffer: &[u8]) -> Option<&str> {
    std::str::from_utf8(&buffer[2..buffer.len() - 1]).ok()
}

/// Decode a modifier mask (`1` + bit set) into modifiers.
fn modifiers_from_mask(mask: u8) -> KeyModifiers {
    let bits = mask.saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    for (bit, modifier) in [
        (1, KeyModifiers::SHIFT),
        (2, KeyModifiers::ALT),
        (4, KeyModifiers::CONTROL),
        (8, KeyModifiers::SUPER),
        (16, KeyModifiers::HYPER),
        (32, KeyModifiers::META),
    ] {
        if bits & bit != 0 {
            modifiers.insert(modifier);
        }
    }
    modifiers
}

/// Decode the lock-key bits of a modifier mask into key state.
const fn state_from_mask(mask: u8) -> KeyEventState {
    let bits = mask.saturating_sub(1);
    let mut state = KeyEventState::NONE;
    if bits & 64 != 0 {
        state.insert(KeyEventState::CAPS_LOCK);
    }
    if bits & 128 != 0 {
        state.insert(KeyEventState::NUM_LOCK);
    }
    state
}

const fn kind_from_code(code: u8) -> KeyEventKind {
    match code {
        2 => KeyEventKind::Repeat,
        3 => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

/// Parse a `mask[:kind]` field; the kind defaults to a press.
fn mask_and_kind(field: Option<&str>) -> Option<(u8, u8)> {
    let mut parts = field?.split(':');
    let mask = parts.next()?.parse::<u8>().ok()?;
    let kind = parts
        .next()
        .and_then(|kind| kind.parse::<u8>().ok())
        .unwrap_or(1);
    Some((mask, kind))
}

/// `CSI [1] ; mask[:kind] {A,B,C,D,F,H,P,Q,R,S}`.
fn parse_modified_key(buffer: &[u8]) -> Parsed {
    let Some(text) = parameters(buffer) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    fields.next();
    let (modifiers, kind) = if let Some((mask, kind)) = mask_and_kind(fields.next()) {
        (modifiers_from_mask(mask), kind_from_code(kind))
    } else if buffer.len() > 3 {
        // `CSI <mask> <final>`: the digit before the final byte is the mask.
        let Some(digit) = char::from(buffer[buffer.len() - 2]).to_digit(10) else {
            return Invalid;
        };
        // A single decimal digit always fits.
        #[allow(clippy::cast_possible_truncation)]
        let mask = digit as u8;
        (modifiers_from_mask(mask), KeyEventKind::Press)
    } else {
        (KeyModifiers::NONE, KeyEventKind::Press)
    };
    let code = match buffer[buffer.len() - 1] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return Invalid,
    };
    key(KeyEvent::new_with_kind(code, modifiers, kind))
}

/// Kitty functional key codepoints from the Unicode private use area.
fn functional_key(codepoint: u32) -> Option<(KeyCode, KeyEventState)> {
    let keypad = match codepoint {
        57399..=57408 => {
            // 57399 is keypad 0; the offset is below 10.
            #[allow(clippy::cast_possible_truncation)]
            let digit = (codepoint - 57399) as u8;
            Some(KeyCode::Char(char::from(b'0' + digit)))
        }
        57409 => Some(KeyCode::Char('.')),
        57410 => Some(KeyCode::Char('/')),
        57411 => Some(KeyCode::Char('*')),
        57412 => Some(KeyCode::Char('-')),
        57413 => Some(KeyCode::Char('+')),
        57414 => Some(KeyCode::Enter),
        57415 => Some(KeyCode::Char('=')),
        57416 => Some(KeyCode::Char(',')),
        57417 => Some(KeyCode::Left),
        57418 => Some(KeyCode::Right),
        57419 => Some(KeyCode::Up),
        57420 => Some(KeyCode::Down),
        57421 => Some(KeyCode::PageUp),
        57422 => Some(KeyCode::PageDown),
        57423 => Some(KeyCode::Home),
        57424 => Some(KeyCode::End),
        57425 => Some(KeyCode::Insert),
        57426 => Some(KeyCode::Delete),
        57427 => Some(KeyCode::KeypadBegin),
        _ => None,
    };
    if let Some(code) = keypad {
        return Some((code, KeyEventState::KEYPAD));
    }
    let code = match codepoint {
        57358 => KeyCode::CapsLock,
        57359 => KeyCode::ScrollLock,
        57360 => KeyCode::NumLock,
        57361 => KeyCode::PrintScreen,
        57362 => KeyCode::Pause,
        57363 => KeyCode::Menu,
        57376..=57398 => {
            // 57376 is F13; the offset is at most 22.
            #[allow(clippy::cast_possible_truncation)]
            let offset = (codepoint - 57376) as u8;
            KeyCode::F(13 + offset)
        }
        57428 => KeyCode::Media(MediaKeyCode::Play),
        57429 => KeyCode::Media(MediaKeyCode::Pause),
        57430 => KeyCode::Media(MediaKeyCode::PlayPause),
        57431 => KeyCode::Media(MediaKeyCode::Reverse),
        57432 => KeyCode::Media(MediaKeyCode::Stop),
        57433 => KeyCode::Media(MediaKeyCode::FastForward),
        57434 => KeyCode::Media(MediaKeyCode::Rewind),
        57435 => KeyCode::Media(MediaKeyCode::TrackNext),
        57436 => KeyCode::Media(MediaKeyCode::TrackPrevious),
        57437 => KeyCode::Media(MediaKeyCode::Record),
        57438 => KeyCode::Media(MediaKeyCode::LowerVolume),
        57439 => KeyCode::Media(MediaKeyCode::RaiseVolume),
        57440 => KeyCode::Media(MediaKeyCode::MuteVolume),
        57441 => KeyCode::Modifier(ModifierKeyCode::LeftShift),
        57442 => KeyCode::Modifier(ModifierKeyCode::LeftControl),
        57443 => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
        57444 => KeyCode::Modifier(ModifierKeyCode::LeftSuper),
        57445 => KeyCode::Modifier(ModifierKeyCode::LeftHyper),
        57446 => KeyCode::Modifier(ModifierKeyCode::LeftMeta),
        57447 => KeyCode::Modifier(ModifierKeyCode::RightShift),
        57448 => KeyCode::Modifier(ModifierKeyCode::RightControl),
        57449 => KeyCode::Modifier(ModifierKeyCode::RightAlt),
        57450 => KeyCode::Modifier(ModifierKeyCode::RightSuper),
        57451 => KeyCode::Modifier(ModifierKeyCode::RightHyper),
        57452 => KeyCode::Modifier(ModifierKeyCode::RightMeta),
        57453 => KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift),
        57454 => KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift),
        _ => return None,
    };
    Some((code, KeyEventState::NONE))
}

/// `CSI codepoint[:alternates] [; mask[:kind] [; text]] u`.
fn parse_csi_u(buffer: &[u8], raw_mode: bool) -> Parsed {
    let Some(text) = parameters(buffer) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    let mut codepoints = fields.next().unwrap_or_default().split(':');
    let Some(codepoint) = codepoints
        .next()
        .and_then(|codepoint| codepoint.parse::<u32>().ok())
    else {
        return Invalid;
    };
    let (mut modifiers, kind, mask_state) = mask_and_kind(fields.next()).map_or(
        (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE),
        |(mask, kind)| {
            (
                modifiers_from_mask(mask),
                kind_from_code(kind),
                state_from_mask(mask),
            )
        },
    );
    let (mut code, key_state) = if let Some(functional) = functional_key(codepoint) {
        functional
    } else if let Some(character) = char::from_u32(codepoint) {
        let code = match character {
            '\x1b' => KeyCode::Esc,
            '\r' => KeyCode::Enter,
            '\n' if !raw_mode => KeyCode::Enter,
            '\t' if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            '\t' => KeyCode::Tab,
            '\x7f' => KeyCode::Backspace,
            _ => KeyCode::Char(character),
        };
        (code, KeyEventState::NONE)
    } else {
        return Invalid;
    };
    // A lone modifier key also counts as holding that modifier.
    if let KeyCode::Modifier(modifier) = code {
        let held = match modifier {
            ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift => KeyModifiers::SHIFT,
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl => KeyModifiers::CONTROL,
            ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt => KeyModifiers::ALT,
            ModifierKeyCode::LeftSuper | ModifierKeyCode::RightSuper => KeyModifiers::SUPER,
            ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => KeyModifiers::HYPER,
            ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => KeyModifiers::META,
            ModifierKeyCode::IsoLevel3Shift | ModifierKeyCode::IsoLevel5Shift => {
                KeyModifiers::NONE
            }
        };
        modifiers.insert(held);
    }
    // With alternate keys reported, a shifted key carries the shifted
    // character, which replaces the base key and absorbs Shift.
    if modifiers.contains(KeyModifiers::SHIFT)
        && let Some(shifted) = codepoints
            .next()
            .and_then(|codepoint| codepoint.parse::<u32>().ok())
            .and_then(char::from_u32)
    {
        code = KeyCode::Char(shifted);
        modifiers.remove(KeyModifiers::SHIFT);
    }
    key(KeyEvent::new_with_kind_and_state(
        code,
        modifiers,
        kind,
        key_state | mask_state,
    ))
}

/// `CSI number [; mask[:kind]] ~`.
fn parse_tilde_key(buffer: &[u8]) -> Parsed {
    let Some(text) = parameters(buffer) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    let Some(number) = fields.next().and_then(|number| number.parse::<u8>().ok()) else {
        return Invalid;
    };
    let (modifiers, kind, state) = mask_and_kind(fields.next()).map_or(
        (KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE),
        |(mask, kind)| {
            (
                modifiers_from_mask(mask),
                kind_from_code(kind),
                state_from_mask(mask),
            )
        },
    );
    let code = match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::F(number - 10),
        17..=21 => KeyCode::F(number - 11),
        23..=26 => KeyCode::F(number - 12),
        28..=29 => KeyCode::F(number - 15),
        31..=34 => KeyCode::F(number - 17),
        _ => return Invalid,
    };
    key(KeyEvent::new_with_kind_and_state(
        code, modifiers, kind, state,
    ))
}

/// `CSI row ; column R`; reported positions are zero-based.
fn parse_cursor_position(buffer: &[u8]) -> Parsed {
    let Some(text) = parameters(buffer) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    let row = fields.next().and_then(|row| row.parse::<u16>().ok());
    let column = fields.next().and_then(|column| column.parse::<u16>().ok());
    match (row, column) {
        (Some(row), Some(column)) => Parsed::Event(InternalEvent::CursorPosition(
            column.wrapping_sub(1),
            row.wrapping_sub(1),
        )),
        _ => Invalid,
    }
}

/// Decode the button byte shared by every mouse encoding.
const fn mouse_button(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    let kind = match (button, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        // Button 3 is "released" in encodings that cannot name the button.
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::NONE;
    if cb & 0b0000_0100 != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if cb & 0b0000_1000 != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    if cb & 0b0001_0000 != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }
    Some((kind, modifiers))
}

const fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Parsed {
    event(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

/// `CSI M Cb Cx Cy`, each byte offset by 32 and positions one-based.
fn parse_x10_mouse(buffer: &[u8]) -> Parsed {
    if buffer.len() < 6 {
        return Incomplete;
    }
    let Some((kind, modifiers)) = buffer[3].checked_sub(32).and_then(mouse_button) else {
        return Invalid;
    };
    let column = u16::from(buffer[4].saturating_sub(32)).wrapping_sub(1);
    let row = u16::from(buffer[5].saturating_sub(32)).wrapping_sub(1);
    mouse(kind, column, row, modifiers)
}

/// `CSI < Cb ; Cx ; Cy {M,m}`, where `m` reports a release.
fn parse_sgr_mouse(buffer: &[u8]) -> Parsed {
    let release = match buffer.last() {
        Some(b'm') => true,
        Some(b'M') => false,
        _ => return Incomplete,
    };
    let Ok(text) = std::str::from_utf8(&buffer[3..buffer.len() - 1]) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    let Some((kind, modifiers)) = fields
        .next()
        .and_then(|cb| cb.parse::<u8>().ok())
        .and_then(mouse_button)
    else {
        return Invalid;
    };
    let (Some(column), Some(row)) = (
        fields.next().and_then(|column| column.parse::<u16>().ok()),
        fields.next().and_then(|row| row.parse::<u16>().ok()),
    ) else {
        return Invalid;
    };
    let kind = match kind {
        MouseEventKind::Down(button) if release => MouseEventKind::Up(button),
        other => other,
    };
    mouse(kind, column.wrapping_sub(1), row.wrapping_sub(1), modifiers)
}

/// `CSI Cb ; Cx ; Cy M` with the button offset by 32.
fn parse_rxvt_mouse(buffer: &[u8]) -> Parsed {
    let Some(text) = parameters(buffer) else {
        return Invalid;
    };
    let mut fields = text.split(';');
    let Some((kind, modifiers)) = fields
        .next()
        .and_then(|cb| cb.parse::<u8>().ok())
        .and_then(|cb| cb.checked_sub(32))
        .and_then(mouse_button)
    else {
        return Invalid;
    };
    let (Some(column), Some(row)) = (
        fields.next().and_then(|column| column.parse::<u16>().ok()),
        fields.next().and_then(|row| row.parse::<u16>().ok()),
    ) else {
        return Invalid;
    };
    mouse(kind, column.wrapping_sub(1), row.wrapping_sub(1), modifiers)
}

/// `CSI 200 ~ text CSI 201 ~`; invalid UTF-8 is replaced.
fn parse_bracketed_paste(buffer: &[u8]) -> Parsed {
    if !buffer.ends_with(b"\x1b[201~") {
        return Incomplete;
    }
    let text = String::from_utf8_lossy(&buffer[6..buffer.len() - 6]).into_owned();
    event(Event::Paste(text))
}

/// One UTF-8 character: `Ok(None)` while a valid sequence is incomplete.
fn parse_utf8_char(buffer: &[u8]) -> Result<Option<char>, ()> {
    if let Ok(text) = std::str::from_utf8(buffer) {
        return text.chars().next().map(Some).ok_or(());
    }
    let required = match buffer[0] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        0x80..=0xbf | 0xf8..=0xff => return Err(()),
    };
    if buffer[1..]
        .iter()
        .any(|byte| byte & 0b1100_0000 != 0b1000_0000)
    {
        return Err(());
    }
    if buffer.len() < required {
        Ok(None)
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8], raw_mode: bool) -> Vec<InternalEvent> {
        let mut parser = Parser::default();
        parser.advance(bytes, false, raw_mode);
        std::iter::from_fn(|| parser.next()).collect()
    }

    fn keys(bytes: &[u8]) -> Vec<KeyEvent> {
        decode_all(bytes, true)
            .into_iter()
            .map(|event| match event {
                InternalEvent::Event(Event::Key(key)) => key,
                other => panic!("expected key, got {other:?}"),
            })
            .collect()
    }

    fn one_key(bytes: &[u8]) -> KeyEvent {
        let decoded = keys(bytes);
        assert_eq!(decoded.len(), 1, "{bytes:?} -> {decoded:?}");
        decoded[0]
    }

    fn one_event(bytes: &[u8]) -> Event {
        let decoded = decode_all(bytes, true);
        assert_eq!(decoded.len(), 1, "{bytes:?} -> {decoded:?}");
        match decoded.into_iter().next().unwrap() {
            InternalEvent::Event(event) => event,
            other => panic!("expected event, got {other:?}"),
        }
    }

    fn k(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn escape_alone_and_with_alt() {
        assert_eq!(one_key(b"\x1b"), k(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(parse(b"\x1b", true, true), Incomplete);
        assert_eq!(one_key(b"\x1b\x1b"), k(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1bc"), k(KeyCode::Char('c'), KeyModifiers::ALT));
        assert_eq!(
            one_key(b"\x1bH"),
            k(KeyCode::Char('H'), KeyModifiers::ALT | KeyModifiers::SHIFT)
        );
        assert_eq!(
            one_key(b"\x1b\x01"),
            k(KeyCode::Char('a'), KeyModifiers::ALT | KeyModifiers::CONTROL)
        );
        assert_eq!(one_key(b"\x1b\r"), k(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            one_key("\x1bé".as_bytes()),
            k(KeyCode::Char('é'), KeyModifiers::ALT)
        );
    }

    #[test]
    fn control_bytes() {
        assert_eq!(one_key(b"\r"), k(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(one_key(b"\t"), k(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x7f"), k(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x00"), k(KeyCode::Char(' '), KeyModifiers::CONTROL));
        assert_eq!(one_key(b"\x03"), k(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(one_key(b"\x08"), k(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert_eq!(one_key(b"\x1c"), k(KeyCode::Char('4'), KeyModifiers::CONTROL));
        assert_eq!(one_key(b"\x1f"), k(KeyCode::Char('7'), KeyModifiers::CONTROL));
        // Line feed is Ctrl+J in raw mode and Enter otherwise.
        assert_eq!(one_key(b"\n"), k(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(
            decode_all(b"\n", false),
            vec![InternalEvent::Event(Event::Key(k(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))]
        );
    }

    #[test]
    fn printable_characters() {
        assert_eq!(one_key(b"a"), k(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(one_key(b"A"), k(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(one_key(b" "), k(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(
            one_key("É".as_bytes()),
            k(KeyCode::Char('É'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            one_key("😀".as_bytes()),
            k(KeyCode::Char('😀'), KeyModifiers::NONE)
        );
        assert_eq!(parse(&[0xe2, 0x82], false, true), Incomplete);
        assert_eq!(parse(&[0xe2, 0x41], false, true), Invalid);
        assert_eq!(parse(&[0x80], false, true), Invalid);
        assert_eq!(keys(&[0xff, b'x']), vec![k(KeyCode::Char('x'), KeyModifiers::NONE)]);
    }

    #[test]
    fn ss3_and_plain_csi_keys() {
        for (bytes, code) in [
            (&b"\x1bOA"[..], KeyCode::Up),
            (b"\x1bOB", KeyCode::Down),
            (b"\x1bOC", KeyCode::Right),
            (b"\x1bOD", KeyCode::Left),
            (b"\x1bOH", KeyCode::Home),
            (b"\x1bOF", KeyCode::End),
            (b"\x1bOP", KeyCode::F(1)),
            (b"\x1bOS", KeyCode::F(4)),
            (b"\x1b[A", KeyCode::Up),
            (b"\x1b[B", KeyCode::Down),
            (b"\x1b[C", KeyCode::Right),
            (b"\x1b[D", KeyCode::Left),
            (b"\x1b[H", KeyCode::Home),
            (b"\x1b[F", KeyCode::End),
            (b"\x1b[P", KeyCode::F(1)),
            (b"\x1b[Q", KeyCode::F(2)),
            (b"\x1b[S", KeyCode::F(4)),
            (b"\x1b[[A", KeyCode::F(1)),
            (b"\x1b[[E", KeyCode::F(5)),
        ] {
            assert_eq!(one_key(bytes), k(code, KeyModifiers::NONE), "{bytes:?}");
        }
        assert_eq!(one_key(b"\x1b[Z"), k(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(decode_all(b"\x1bOx", true), Vec::<InternalEvent>::new());
    }

    #[test]
    fn modified_and_tilde_keys() {
        assert_eq!(
            one_key(b"\x1b[1;5A"),
            k(KeyCode::Up, KeyModifiers::CONTROL)
        );
        assert_eq!(
            one_key(b"\x1b[1;2D"),
            k(KeyCode::Left, KeyModifiers::SHIFT)
        );
        assert_eq!(
            one_key(b"\x1b[1;5:3C"),
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::CONTROL, KeyEventKind::Release)
        );
        assert_eq!(one_key(b"\x1b[5A"), k(KeyCode::Up, KeyModifiers::CONTROL));
        // `ESC [ ;` is classified as soon as `;` arrives and is discarded.
        assert_eq!(
            keys(b"\x1b[;3B"),
            vec![
                k(KeyCode::Char('3'), KeyModifiers::NONE),
                k(KeyCode::Char('B'), KeyModifiers::SHIFT)
            ]
        );
        for (bytes, code) in [
            (&b"\x1b[1~"[..], KeyCode::Home),
            (b"\x1b[2~", KeyCode::Insert),
            (b"\x1b[3~", KeyCode::Delete),
            (b"\x1b[4~", KeyCode::End),
            (b"\x1b[5~", KeyCode::PageUp),
            (b"\x1b[6~", KeyCode::PageDown),
            (b"\x1b[7~", KeyCode::Home),
            (b"\x1b[8~", KeyCode::End),
            (b"\x1b[11~", KeyCode::F(1)),
            (b"\x1b[15~", KeyCode::F(5)),
            (b"\x1b[17~", KeyCode::F(6)),
            (b"\x1b[21~", KeyCode::F(10)),
            (b"\x1b[23~", KeyCode::F(11)),
            (b"\x1b[24~", KeyCode::F(12)),
            (b"\x1b[26~", KeyCode::F(14)),
            (b"\x1b[28~", KeyCode::F(13)),
            (b"\x1b[29~", KeyCode::F(14)),
            (b"\x1b[31~", KeyCode::F(14)),
            (b"\x1b[34~", KeyCode::F(17)),
        ] {
            assert_eq!(one_key(bytes), k(code, KeyModifiers::NONE), "{bytes:?}");
        }
        assert_eq!(
            one_key(b"\x1b[3;5~"),
            k(KeyCode::Delete, KeyModifiers::CONTROL)
        );
        assert_eq!(decode_all(b"\x1b[99~", true), Vec::<InternalEvent>::new());
    }

    #[test]
    fn csi_u_keys() {
        assert_eq!(one_key(b"\x1b[97u"), k(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(
            one_key(b"\x1b[97;5u"),
            k(KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            one_key(b"\x1b[97;5:2u"),
            KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat
            )
        );
        assert_eq!(one_key(b"\x1b[27u"), k(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1b[13u"), k(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1b[127u"), k(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1b[9u"), k(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1b[9;2u"), k(KeyCode::BackTab, KeyModifiers::SHIFT));
        // The shifted alternate replaces the key and absorbs Shift.
        assert_eq!(
            one_key(b"\x1b[49:33;2u"),
            k(KeyCode::Char('!'), KeyModifiers::NONE)
        );
        assert_eq!(
            one_key(b"\x1b[49:33;6u"),
            k(KeyCode::Char('!'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            one_key(b"\x1b[97;2u"),
            k(KeyCode::Char('a'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            one_key(b"\x1b[57404u"),
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char('5'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                KeyEventState::KEYPAD
            )
        );
        assert_eq!(one_key(b"\x1b[57376u"), k(KeyCode::F(13), KeyModifiers::NONE));
        assert_eq!(one_key(b"\x1b[57398u"), k(KeyCode::F(35), KeyModifiers::NONE));
        assert_eq!(
            one_key(b"\x1b[57441u"),
            k(
                KeyCode::Modifier(ModifierKeyCode::LeftShift),
                KeyModifiers::SHIFT
            )
        );
        assert_eq!(
            one_key(b"\x1b[97;65u"),
            KeyEvent::new_with_kind_and_state(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK
            )
        );
        assert_eq!(decode_all(b"\x1b[1114112u", true), Vec::<InternalEvent>::new());
    }

    #[test]
    fn mouse_encodings() {
        assert_eq!(
            one_event(b"\x1b[<0;20;10M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 9,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[<2;1;1m"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Right),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[<35;5;5M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[<64;3;4M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 2,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[<28;3;4M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 3,
                modifiers: KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
            })
        );
        assert_eq!(
            one_event(b"\x1b[<67;3;4M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollRight,
                column: 2,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[M\x20\x21\x22"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(
            one_event(b"\x1b[32;30;40M"),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 29,
                row: 39,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert_eq!(decode_all(b"\x1b[<200;1;1M", true), Vec::<InternalEvent>::new());
    }

    #[test]
    fn focus_paste_and_replies() {
        assert_eq!(one_event(b"\x1b[I"), Event::FocusGained);
        assert_eq!(one_event(b"\x1b[O"), Event::FocusLost);
        assert_eq!(
            one_event(b"\x1b[200~hello\r\nwor\x1bld\x1b[201~"),
            Event::Paste("hello\r\nwor\x1bld".to_owned())
        );
        assert_eq!(
            one_event(b"\x1b[200~\xff\x1b[201~"),
            Event::Paste("\u{fffd}".to_owned())
        );
        assert_eq!(
            decode_all(b"\x1b[?1u\x1b[?62;22c\x1b[5;10R", true),
            vec![
                InternalEvent::KeyboardEnhancementFlags,
                InternalEvent::PrimaryDeviceAttributes,
                InternalEvent::CursorPosition(9, 4),
            ]
        );
    }

    #[test]
    fn sequences_split_across_chunks_reassemble() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b[", true, true);
        parser.advance(b"1;5", true, true);
        assert!(parser.next().is_none());
        parser.advance(b"A", false, true);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(k(
                KeyCode::Up,
                KeyModifiers::CONTROL
            ))))
        );
    }

    #[test]
    fn unknown_sequences_are_discarded_and_decoding_resumes() {
        assert_eq!(keys(b"\x1b[1;5Xa"), vec![k(KeyCode::Char('a'), KeyModifiers::NONE)]);
        assert_eq!(keys(b"\x1b[!a"), vec![k(KeyCode::Char('a'), KeyModifiers::NONE)]);
    }
}
