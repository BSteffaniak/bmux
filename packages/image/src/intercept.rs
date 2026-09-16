//! Image sequence interceptor.
//!
//! Sits between the raw PTY reader and terminal parser. Detects and
//! extracts image escape sequences (Sixel DCS, Kitty APC, iTerm2 OSC 1337)
//! from the byte stream, returning filtered bytes (images stripped) and
//! structured [`ImageEvent`]s.

use crate::model::{ImageEvent, ImagePosition};

const DEFAULT_MAX_ENCODED_IMAGE_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Interceptor state machine
// ---------------------------------------------------------------------------

/// Internal parse state for the image interceptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Normal ground state — scanning for escape sequences.
    Ground,
    /// Saw ESC (`0x1B`), waiting for the next byte.
    Escape,

    // -- Sixel (DCS) path ------------------------------------------------
    /// Inside a DCS sequence, accumulating the intermediary/final byte.
    #[cfg(feature = "sixel")]
    DcsEntry,
    /// Inside a sixel DCS body (`ESC P q ...`), accumulating image data.
    #[cfg(feature = "sixel")]
    SixelBody,
    /// Saw ESC inside a sixel body — looking for `\` to form ST.
    #[cfg(feature = "sixel")]
    SixelEscape,

    // -- Kitty (APC) path ------------------------------------------------
    /// Inside an APC sequence (`ESC _ G ...`), accumulating payload.
    #[cfg(feature = "kitty")]
    KittyBody,
    /// Saw ESC inside kitty APC body — looking for `\` to form ST.
    #[cfg(feature = "kitty")]
    KittyEscape,

    // -- iTerm2 (OSC 1337) path ------------------------------------------
    /// Inside an OSC sequence, checking for `1337;File=`.
    #[cfg(feature = "iterm2")]
    OscEntry,
    /// Inside an iTerm2 OSC 1337 body, accumulating image data.
    #[cfg(feature = "iterm2")]
    ITerm2Body,
    /// Saw ESC inside iTerm2 body — looking for `\` to form ST.
    #[cfg(feature = "iterm2")]
    ITerm2Escape,
}

/// Result of processing a chunk of PTY output through the interceptor.
pub struct InterceptResult {
    /// Bytes with image sequences removed. Feed this to the terminal parser.
    pub filtered: Vec<u8>,
    /// Image events extracted from the stream.
    pub events: Vec<ImageEvent>,
}

/// Detects and extracts image escape sequences from raw PTY output.
///
/// Designed as a concrete struct today; the public API surface matches a
/// trait shape suitable for future plugin extraction.
pub struct ImageInterceptor {
    state: State,
    buf: Vec<u8>,
    #[allow(dead_code)]
    capture_position: ImagePosition,
    /// Byte offset in the filtered output when the current image's ESC was seen.
    capture_filtered_offset: usize,
    max_encoded_image_bytes: usize,
    discarding_oversized: bool,

    #[cfg(feature = "sixel")]
    dcs_intermediates: Vec<u8>,

    #[cfg(feature = "iterm2")]
    osc_prefix: Vec<u8>,
}

impl ImageInterceptor {
    /// Create a new interceptor.
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::with_capacity(4096),
            capture_position: ImagePosition { row: 0, col: 0 },
            capture_filtered_offset: 0,
            max_encoded_image_bytes: DEFAULT_MAX_ENCODED_IMAGE_BYTES,
            discarding_oversized: false,
            #[cfg(feature = "sixel")]
            dcs_intermediates: Vec::new(),
            #[cfg(feature = "iterm2")]
            osc_prefix: Vec::new(),
        }
    }

    /// Create an interceptor with an encoded image sequence byte limit.
    #[must_use]
    pub fn with_max_encoded_bytes(max_encoded_image_bytes: usize) -> Self {
        Self {
            max_encoded_image_bytes: max_encoded_image_bytes.max(1),
            ..Self::new()
        }
    }

    fn push_image_byte(&mut self, byte: u8) {
        if self.discarding_oversized {
            return;
        }
        if self.buf.len() >= self.max_encoded_image_bytes {
            self.buf.clear();
            self.discarding_oversized = true;
        } else {
            self.buf.push(byte);
        }
    }

    fn take_image_bytes(&mut self) -> Option<Vec<u8>> {
        if self.discarding_oversized {
            self.discarding_oversized = false;
            self.buf.clear();
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }

    /// Process a chunk of raw PTY output bytes.
    ///
    /// Returns filtered bytes (images stripped) and image events with
    /// `filtered_byte_offset` set.  The caller should use each event's
    /// offset to feed filtered bytes to the cursor tracker and capture
    /// the cursor position at the right moment, then call
    /// `event.set_position(pos)` before passing to the registry.
    pub fn process(&mut self, input: &[u8]) -> InterceptResult {
        // Bytes preceding a sequence split across reads were already delivered
        // in the previous result. Its offset is relative to this new chunk.
        self.capture_filtered_offset = 0;
        let mut filtered = Vec::with_capacity(input.len());
        #[allow(unused_mut)]
        let mut events = Vec::new();

        for &byte in input {
            if matches!(byte, 0x18 | 0x1A) {
                // CAN/SUB cancel an in-flight control string, including one
                // being discarded for exceeding its budget. Preserve the
                // control byte so the downstream parser cancels its state too.
                self.reset();
                filtered.push(byte);
                continue;
            }
            // ESC ends an unfinished control string. Unless followed by ST,
            // interpret it as the start of the next escape sequence instead of
            // appending subsequent terminal controls to the image payload.
            let string_escape = match self.state {
                #[cfg(feature = "sixel")]
                State::SixelEscape => true,
                #[cfg(feature = "kitty")]
                State::KittyEscape => true,
                #[cfg(feature = "iterm2")]
                State::ITerm2Escape => true,
                _ => false,
            };
            if string_escape && byte != b'\\' {
                self.reset();
                self.state = State::Escape;
                self.capture_filtered_offset = filtered.len();
            }
            match self.state {
                State::Ground => {
                    if byte == 0x1B {
                        self.state = State::Escape;
                        // Record the filtered byte count at the ESC that
                        // starts a potential image sequence.
                        self.capture_filtered_offset = filtered.len();
                    } else {
                        filtered.push(byte);
                    }
                }

                State::Escape => {
                    match byte {
                        // DCS: ESC P — potential sixel
                        #[cfg(feature = "sixel")]
                        b'P' => {
                            self.state = State::DcsEntry;
                            self.dcs_intermediates.clear();
                            self.buf.clear();
                            // Position will be set by the caller using filtered_byte_offset.
                            self.capture_position = ImagePosition { row: 0, col: 0 };
                        }

                        // APC: ESC _ — potential kitty graphics
                        #[cfg(feature = "kitty")]
                        b'_' => {
                            self.state = State::KittyBody;
                            self.buf.clear();
                            self.discarding_oversized = false;
                            self.capture_position = ImagePosition { row: 0, col: 0 };
                        }

                        // OSC: ESC ] — potential iTerm2 inline image
                        #[cfg(feature = "iterm2")]
                        b']' => {
                            self.state = State::OscEntry;
                            self.osc_prefix.clear();
                            self.buf.clear();
                            self.capture_position = ImagePosition { row: 0, col: 0 };
                        }

                        0x1B => {
                            // Consecutive ESC bytes restart the escape parser.
                            filtered.push(0x1B);
                            self.capture_filtered_offset = filtered.len();
                        }
                        // Not an image-related sequence — pass through ESC + byte
                        _ => {
                            filtered.push(0x1B);
                            filtered.push(byte);
                            self.state = State::Ground;
                        }
                    }
                }

                // -- Sixel DCS path -------------------------------------------
                #[cfg(feature = "sixel")]
                State::DcsEntry => {
                    // Accumulate DCS parameter/intermediate bytes until the
                    // final byte.  Sixel's final byte is 'q'.
                    if (0x20..=0x3F).contains(&byte) {
                        if self.dcs_intermediates.len() >= self.max_encoded_image_bytes {
                            // Discard the entire oversized DCS through ST, not
                            // just its header: its body must not become text.
                            self.dcs_intermediates.clear();
                            self.buf.clear();
                            self.discarding_oversized = true;
                            self.state = State::SixelBody;
                        } else {
                            self.dcs_intermediates.push(byte);
                        }
                    } else if byte == b'q' {
                        // Final byte = sixel!  Enter body accumulation.
                        self.state = State::SixelBody;
                        self.buf.clear();
                        self.discarding_oversized = false;
                    } else {
                        // Not sixel — pass through the original DCS sequence.
                        filtered.push(0x1B);
                        filtered.push(b'P');
                        filtered.extend_from_slice(&self.dcs_intermediates);
                        filtered.push(byte);
                        self.dcs_intermediates.clear();
                        self.state = State::Ground;
                    }
                }

                #[cfg(feature = "sixel")]
                State::SixelBody => {
                    if byte == 0x1B {
                        self.state = State::SixelEscape;
                    } else {
                        self.push_image_byte(byte);
                    }
                }

                #[cfg(feature = "sixel")]
                State::SixelEscape => {
                    if byte == b'\\' {
                        if let Some(data) = self.take_image_bytes() {
                            let pixel_size = crate::codec::sixel::estimate_pixel_size(&data);
                            events.push(ImageEvent::SixelImage {
                                data,
                                position: self.capture_position,
                                pixel_size,
                                filtered_byte_offset: self.capture_filtered_offset,
                            });
                        }
                        self.state = State::Ground;
                    } else {
                        // False alarm — ESC was part of the data.
                        self.push_image_byte(0x1B);
                        self.push_image_byte(byte);
                        self.state = State::SixelBody;
                    }
                }

                // -- Kitty APC path -------------------------------------------
                #[cfg(feature = "kitty")]
                State::KittyBody => {
                    match byte {
                        0x1B => self.state = State::KittyEscape,
                        0x07 => {
                            // BEL terminates APC in some terminals.
                            if let Some(body) = self.take_image_bytes()
                                && let Some(cmd) =
                                    crate::codec::kitty::parse_command(&body, self.capture_position)
                            {
                                events.push(ImageEvent::KittyCommand {
                                    command: cmd,
                                    filtered_byte_offset: self.capture_filtered_offset,
                                });
                            }
                            self.state = State::Ground;
                        }
                        _ => {
                            // Only accumulate if it starts with 'G' (kitty graphics).
                            if self.buf.is_empty() && !self.discarding_oversized && byte != b'G' {
                                // Not a kitty graphics APC — pass through.
                                filtered.push(0x1B);
                                filtered.push(b'_');
                                filtered.push(byte);
                                self.state = State::Ground;
                            } else {
                                self.push_image_byte(byte);
                            }
                        }
                    }
                }

                #[cfg(feature = "kitty")]
                State::KittyEscape => {
                    if byte == b'\\' {
                        // ST — kitty command complete.
                        if let Some(body) = self.take_image_bytes()
                            && let Some(cmd) =
                                crate::codec::kitty::parse_command(&body, self.capture_position)
                        {
                            events.push(ImageEvent::KittyCommand {
                                command: cmd,
                                filtered_byte_offset: self.capture_filtered_offset,
                            });
                        }
                        self.state = State::Ground;
                    } else {
                        self.push_image_byte(0x1B);
                        self.push_image_byte(byte);
                        self.state = State::KittyBody;
                    }
                }

                // -- iTerm2 OSC path ------------------------------------------
                #[cfg(feature = "iterm2")]
                State::OscEntry => {
                    const PREFIX: &[u8] = b"1337;File=";
                    self.osc_prefix.push(byte);

                    if self.osc_prefix.len() <= PREFIX.len() {
                        if PREFIX[self.osc_prefix.len() - 1] == byte {
                            if self.osc_prefix.len() == PREFIX.len() {
                                // Matched "1337;File=" — enter body.
                                self.state = State::ITerm2Body;
                                self.buf.clear();
                                self.discarding_oversized = false;
                            }
                            // else: keep accumulating prefix bytes
                        } else {
                            // Prefix mismatch — not an iTerm2 image OSC.
                            filtered.push(0x1B);
                            filtered.push(b']');
                            filtered.extend_from_slice(&self.osc_prefix);
                            self.osc_prefix.clear();
                            self.state = State::Ground;
                        }
                    } else {
                        // Prefix too long — not an iTerm2 image.
                        filtered.push(0x1B);
                        filtered.push(b']');
                        filtered.extend_from_slice(&self.osc_prefix);
                        self.osc_prefix.clear();
                        self.state = State::Ground;
                    }
                }

                #[cfg(feature = "iterm2")]
                State::ITerm2Body => {
                    match byte {
                        0x1B => self.state = State::ITerm2Escape,
                        0x07 => {
                            // BEL terminates OSC.
                            if let Some(data) = self.take_image_bytes() {
                                events.push(ImageEvent::ITerm2Image {
                                    data,
                                    position: self.capture_position,
                                    filtered_byte_offset: self.capture_filtered_offset,
                                });
                            }
                            self.state = State::Ground;
                        }
                        _ => self.push_image_byte(byte),
                    }
                }

                #[cfg(feature = "iterm2")]
                State::ITerm2Escape => {
                    if byte == b'\\' {
                        if let Some(data) = self.take_image_bytes() {
                            events.push(ImageEvent::ITerm2Image {
                                data,
                                position: self.capture_position,
                                filtered_byte_offset: self.capture_filtered_offset,
                            });
                        }
                        self.state = State::Ground;
                    } else {
                        self.push_image_byte(0x1B);
                        self.push_image_byte(byte);
                        self.state = State::ITerm2Body;
                    }
                }
            }
        }

        InterceptResult { filtered, events }
    }

    /// Reset to ground state, discarding any partially-accumulated data.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.discarding_oversized = false;
        #[cfg(feature = "sixel")]
        self.dcs_intermediates.clear();
        #[cfg(feature = "iterm2")]
        self.osc_prefix.clear();
    }
}

impl Default for ImageInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sixel")]
    #[test]
    fn oversized_dcs_header_is_bounded_and_recovers() {
        for final_byte in [b'q', b'p'] {
            let mut wire = b"before\x1bP12345".to_vec();
            wire.push(final_byte);
            wire.extend_from_slice(b"payload\x1b\\after\x1bPq~\x1b\\");
            for split in 0..=wire.len() {
                let mut interceptor = ImageInterceptor::with_max_encoded_bytes(4);
                let mut filtered = Vec::new();
                let mut events = Vec::new();
                for chunk in [&wire[..split], &wire[split..]] {
                    let result = interceptor.process(chunk);
                    assert!(interceptor.dcs_intermediates.len() <= 4);
                    assert!(interceptor.buf.len() <= 4);
                    filtered.extend(result.filtered);
                    events.extend(result.events);
                }
                assert_eq!(filtered, b"beforeafter", "split {split}");
                assert_eq!(events.len(), 1, "split {split}");
                assert!(matches!(&events[0], ImageEvent::SixelImage { data, .. } if data == b"~"));
            }
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn oversized_kitty_sequence_is_discarded_and_text_resumes() {
        let mut interceptor = ImageInterceptor::with_max_encoded_bytes(8);
        let mut input = b"before\x1b_Ga=t,i=1,f=100;".to_vec();
        input.extend_from_slice(&[b'A'; 64]);
        input.extend_from_slice(b"\x1b\\after");

        let result = interceptor.process(&input);

        assert_eq!(result.filtered, b"beforeafter");
        assert!(result.events.is_empty());
    }

    #[test]
    fn passthrough_non_image_data() {
        let mut interceptor = ImageInterceptor::new();
        let input = b"hello world\x1b[31mred\x1b[0m";
        let result = interceptor.process(input);
        assert_eq!(result.filtered, input.to_vec());
        assert!(result.events.is_empty());
    }

    #[cfg(feature = "sixel")]
    #[test]
    fn extract_sixel_image() {
        let mut interceptor = ImageInterceptor::new();
        // ESC P q <body> ESC \
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1bPq");
        input.extend_from_slice(b"#0;2;0;0;0~-"); // minimal sixel body
        input.extend_from_slice(b"\x1b\\");
        input.extend_from_slice(b"after");

        let result = interceptor.process(&input);
        assert_eq!(result.filtered, b"after");
        assert_eq!(result.events.len(), 1);
        match &result.events[0] {
            ImageEvent::SixelImage {
                filtered_byte_offset,
                ..
            } => {
                // Position is (0,0) placeholder; caller resolves via offset.
                assert_eq!(*filtered_byte_offset, 0); // ESC was at start, no filtered bytes before it.
            }
            #[allow(unreachable_patterns)]
            _ => panic!("expected SixelImage event"),
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn non_graphics_apc_passed_through() {
        let mut interceptor = ImageInterceptor::new();
        // ESC _ X ... ESC \  (not 'G', so not kitty graphics)
        let input = b"\x1b_Xhello\x1b\\";
        let result = interceptor.process(input);
        // The ESC _ X should be passed through, then "hello\x1b\\" are ground bytes
        assert!(!result.filtered.is_empty());
        assert!(result.events.is_empty());
    }

    #[cfg(feature = "iterm2")]
    #[test]
    fn extract_iterm2_image() {
        let mut interceptor = ImageInterceptor::new();
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]1337;File=");
        input.extend_from_slice(b"inline=1:AAAA");
        input.push(0x07); // BEL terminator
        input.extend_from_slice(b"after");

        let result = interceptor.process(&input);
        assert_eq!(result.filtered, b"after");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].filtered_byte_offset(), 0);
    }
}
