//! Raw byte stream decoder.
//!
//! Decodes raw terminal byte streams into key events, supporting both
//! legacy VT/xterm escape sequences and optionally CSI u (kitty keyboard
//! protocol) sequences when the `csi-u` feature is enabled on `bmux_keyboard`.

#[cfg(feature = "kitty-keyboard")]
use bmux_keyboard::csi_u;
use bmux_keyboard::legacy;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::{DecodedStroke, InputEvent};

/// Streaming byte decoder that accumulates incomplete sequences.
#[derive(Debug, Default)]
pub(super) struct ByteDecoder {
    pending: Vec<u8>,
}

impl ByteDecoder {
    /// Feed raw bytes and return any decoded input events.
    pub(super) fn feed_events(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();

        loop {
            let Some((stroke, consumed)) = decode_one(&self.pending) else {
                break;
            };
            self.pending.drain(0..consumed);
            events.push(InputEvent::Key(stroke));
        }

        events
    }
}

/// Attempt to decode one key event from the byte buffer.
///
/// Returns `None` if the buffer is empty or contains an incomplete sequence.
fn decode_one(bytes: &[u8]) -> Option<(DecodedStroke, usize)> {
    if bytes.is_empty() {
        return None;
    }

    let first = bytes[0];
    if first != 0x1b {
        let (stroke, raw) = legacy::decode_single(first);
        return Some((DecodedStroke { stroke, raw }, 1));
    }

    if bytes.len() == 1 {
        return Some((
            DecodedStroke {
                stroke: KeyStroke::simple(KeyCode::Escape),
                raw: vec![0x1b],
            },
            1,
        ));
    }

    // Try CSI u decoding first (for kitty keyboard protocol sequences).
    #[cfg(feature = "kitty-keyboard")]
    if let Some(result) = csi_u::decode(bytes) {
        return Some((
            DecodedStroke {
                stroke: result.stroke,
                raw: bytes[..result.consumed].to_vec(),
            },
            result.consumed,
        ));
    }

    // Try legacy escape sequence decoding.
    if let Some(result) = legacy::decode_escape(bytes) {
        return Some((
            DecodedStroke {
                stroke: result.stroke,
                raw: result.raw,
            },
            result.consumed,
        ));
    }

    // If we have ESC + [ or ESC + O, the legacy decoder returned None meaning
    // it's an incomplete sequence. Wait for more data.
    let second = bytes[1];
    if second == b'[' || second == b'O' {
        return None;
    }

    // ESC + non-CSI/SS3 byte: Alt + decoded single byte.
    let (mut decoded_stroke, _) = legacy::decode_single(second);
    decoded_stroke.modifiers.alt = true;
    let raw = vec![0x1b, second];
    Some((
        DecodedStroke {
            stroke: decoded_stroke,
            raw,
        },
        2,
    ))
}
